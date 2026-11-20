use super::PickleError;

pub(super) fn quoted_ascii(line: &[u8]) -> Result<String, PickleError> {
    if line.len() < 2 || !matches!(line[0], b'\'' | b'"') || line.last() != Some(&line[0]) {
        return Err(PickleError::Constructor {
            class: "UnpicklingError",
            message: "the STRING opcode argument must be quoted".into(),
        });
    }
    let bytes = &line[1..line.len() - 1];
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let start = index - 1;
        let Some(&escaped) = bytes.get(index) else {
            return Err(PickleError::Constructor { class: "ValueError", message: "Trailing \\ in string".into() });
        };
        index += 1;
        let byte = match escaped {
            b'\\' | b'\'' | b'"' => escaped,
            b'a' => 7,
            b'b' => 8,
            b't' => 9,
            b'n' => 10,
            b'v' => 11,
            b'f' => 12,
            b'r' => 13,
            b'0'..=b'7' => {
                let mut value = u16::from(escaped - b'0');
                for _ in 0..2 {
                    if let Some(&next @ b'0'..=b'7') = bytes.get(index) {
                        value = (value << 3) | u16::from(next - b'0');
                        index += 1;
                    } else {
                        break;
                    }
                }
                value as u8
            }
            b'x' => {
                let digits = bytes.get(index..index + 2).and_then(|digits| {
                    Some(char::from(digits[0]).to_digit(16)? * 16 + char::from(digits[1]).to_digit(16)?)
                });
                let Some(value) = digits else {
                    return Err(PickleError::Constructor {
                        class: "ValueError",
                        message: format!("invalid \\x escape at position {start}"),
                    });
                };
                index += 2;
                value as u8
            }
            _ => {
                decoded.push(b'\\');
                escaped
            }
        };
        decoded.push(byte);
    }
    if let Some(index) = decoded.iter().position(|byte| !byte.is_ascii()) {
        return Err(PickleError::Constructor {
            class: "UnicodeDecodeError",
            message: format!(
                "'ascii' codec can't decode byte 0x{:02x} in position {index}: ordinal not in range(128)",
                decoded[index]
            ),
        });
    }
    Ok(String::from_utf8(decoded).expect("ASCII decoded bytes"))
}

pub(super) fn raw_unicode_value(bytes: &[u8]) -> Result<super::LegacyValue, PickleError> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if byte != b'\\' || index == bytes.len() {
            result.push(u32::from(byte));
            continue;
        }
        let start = index - 1;
        let escape = bytes[index];
        index += 1;
        let count = match escape {
            b'u' => 4,
            b'U' => 8,
            _ => {
                result.push(u32::from('\\'));
                result.push(u32::from(escape));
                continue;
            }
        };
        let mut codepoint = 0;
        let reason = if escape == b'u' { "truncated \\uXXXX escape" } else { "truncated \\UXXXXXXXX escape" };
        for _ in 0..count {
            let digit = bytes.get(index).and_then(|byte| char::from(*byte).to_digit(16));
            let Some(digit) = digit else {
                return Err(codec_error(bytes, start, index, reason));
            };
            codepoint = (codepoint << 4) | digit;
            index += 1;
        }
        if codepoint > 0x10ffff {
            return Err(codec_error(bytes, start, index, "\\Uxxxxxxxx out of range"));
        }
        result.push(codepoint);
    }
    Ok(super::LegacyValue::from_python_string(
        super::PythonString::from_codepoints(result).expect("validated codepoints"),
    ))
}

fn codec_error(bytes: &[u8], start: usize, end: usize, reason: &str) -> PickleError {
    let location = if end == start + 1 {
        format!("byte 0x{:02x} in position {start}", bytes[start])
    } else {
        format!("bytes in position {start}-{}", end - 1)
    };
    PickleError::Constructor {
        class: "UnicodeDecodeError",
        message: format!("'rawunicodeescape' codec can't decode {location}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{decode, LegacyValue};

    #[test]
    fn protocol_zero_unicode_decodes_latin1_and_unicode_escapes() {
        for (bytes, expected) in [
            (&b"V\xe9\n."[..], "é"),
            (&b"V\xc3\xa9\n."[..], "Ã©"),
            (&b"V\\u263a\\U0001f600\n."[..], "☺😀"),
            (&b"Vone\\u000atwo\\u005cthree\n."[..], "one\ntwo\\three"),
            (&b"V\\n\\x41\\\\u263a\\\n."[..], "\\n\\x41\\\\u263a\\"),
            (&b"V\n."[..], ""),
        ] {
            assert_eq!(decode(bytes), Ok(LegacyValue::Str(expected.into())), "{bytes:?}");
        }
    }

    #[test]
    fn malformed_protocol_zero_unicode_keeps_codec_error_offsets() {
        for (bytes, message) in [
            (
                &b"V\\u12\n."[..],
                "'rawunicodeescape' codec can't decode bytes in position 0-3: truncated \\uXXXX escape",
            ),
            (
                &b"Vx\\u12z4\n."[..],
                "'rawunicodeescape' codec can't decode bytes in position 1-4: truncated \\uXXXX escape",
            ),
            (
                &b"V\\U00110000\n."[..],
                "'rawunicodeescape' codec can't decode bytes in position 0-9: \\Uxxxxxxxx out of range",
            ),
        ] {
            assert_eq!(decode(bytes).unwrap_err().native_exception(), Some(("UnicodeDecodeError", message.into())));
        }
    }

    #[test]
    fn protocol_zero_string_reads_quoted_escaped_ascii() {
        for (bytes, expected) in [
            (&b"S'hello'\n."[..], "hello"),
            (&b"S\"one\\ntwo\\t\\x41\\101\"\n."[..], "one\ntwo\tAA"),
            (&b"S'\\u263a'\n."[..], "\\u263a"),
            (&b"S'\\400'\n."[..], "\0"),
        ] {
            assert_eq!(decode(bytes), Ok(LegacyValue::Str(expected.into())));
        }
        for (bytes, message) in [
            (&b"Splain\n."[..], "the STRING opcode argument must be quoted"),
            (&b"S'\\x4'\n."[..], "invalid \\x escape at position 0"),
            (&b"S'abc\\'\n."[..], "Trailing \\ in string"),
        ] {
            let class = if bytes == b"Splain\n." { "UnpicklingError" } else { "ValueError" };
            assert_eq!(decode(bytes).unwrap_err().native_exception(), Some((class, message.into())));
        }
    }
}
