//! EA Sports WRC: a `wrc_srs` packet in `telemetry\config.json` plus the `wrc_srs.json` structure file.

use std::fs;
use std::path::PathBuf;

use super::env::{PatchEnvironment, PortKey, UserFolder, UserPath};
use super::pyjson::{self, Dict, DumpStyle, PyValue};
use super::pyunicode::{self, IntParseError, PyInt};
use super::text::{self, Encoding, Newline};
use super::PatchError;

const CONFIG: UserPath = UserPath::new(UserFolder::Documents, "My Games\\WRC\\telemetry\\config.json");
const STRUCTURE: UserPath = UserPath::new(UserFolder::Documents, "My Games\\WRC\\telemetry\\udp\\wrc_srs.json");
/// Structure file of the asset library.
const STRUCTURE_TEMPLATE: &str = "wrc_srs.json";
const STRUCTURE_ID: &str = "wrc_srs";

/// The packet entry openrigcue adds.
fn packet_entry(port: PyInt) -> PyValue {
    PyValue::Dict(Dict::of([
        ("structure", PyValue::str(STRUCTURE_ID)),
        ("packet", PyValue::str("session_update")),
        ("ip", PyValue::str("127.0.0.1")),
        ("port", PyValue::Int(port)),
        ("frequencyHz", PyValue::int(-1)),
        ("bEnabled", PyValue::Bool(true)),
    ]))
}

/// This conversion precedes even the installed-file check in both `patched` and `patch`.
fn configured_entry(env: &PatchEnvironment<'_>) -> Result<PyValue, PatchError> {
    use openrigcue_model::native_value::LegacyValue;
    let value = env.port_value(PortKey::Wrc2023)?;
    let failure = |class_name, message| PatchError::PortInteger { class_name, message };
    let parse = |points: Vec<u32>| {
        pyunicode::parse_int(points).map_err(|error| match error {
            IntParseError::InvalidLiteral => {
                let shown: String = openrigcue_model::python_repr::repr(&value).chars().take(200).collect();
                failure("ValueError", format!("invalid literal for int() with base 10: {shown}"))
            }
            IntParseError::TooManyDigits(count) => failure("ValueError", format!(
                "Exceeds the limit (4300 digits) for integer string conversion: value has {count} digits; use sys.set_int_max_str_digits() to increase the limit"
            )),
        })
    };
    let port = match &value {
        LegacyValue::Bool(value) => PyInt::Small(i128::from(*value)),
        LegacyValue::Int(value) => PyInt::Small(i128::from(*value)),
        LegacyValue::Float(value) => PyInt::from_float(*value).ok_or_else(|| {
            if value.is_nan() {
                failure("ValueError", "cannot convert float NaN to integer".to_owned())
            } else {
                failure("OverflowError", "cannot convert float infinity to integer".to_owned())
            }
        })?,
        LegacyValue::Str(text) => parse(text.chars().map(u32::from).collect())?,
        LegacyValue::Bytes(bytes) => {
            // bytes use C whitespace/ASCII digits, without the Unicode decimal transformation.
            let points = bytes.iter().map(|&byte| if byte.is_ascii() { u32::from(byte) } else { 0xffff }).collect();
            parse(points)?
        }
        other => {
            return Err(failure(
                "TypeError",
                format!(
                    "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                    other.type_name()
                ),
            ))
        }
    };
    Ok(packet_entry(port))
}

fn paths(env: &PatchEnvironment<'_>) -> Option<(PathBuf, PathBuf)> {
    let prefix = env.prefix?;
    Some((CONFIG.resolve(prefix), STRUCTURE.resolve(prefix)))
}

/// `telemetry\config.json` exists.
pub fn installed(env: &PatchEnvironment<'_>) -> bool {
    paths(env).is_some_and(|(config, _)| config.is_file())
}

/// The first `wrc_srs` packet equals the entry (loose equality) and the structure file's text equals the library
/// copy; any parse or lookup failure is false.
pub fn patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let entry = configured_entry(env)?;
    let Some((config, structure)) = paths(env).filter(|(config, _)| config.is_file()) else {
        return Ok(false);
    };
    // A file that cannot be opened is an error; a failure reading or parsing it is false.
    let bytes = fs::read(&config)?;
    let Some(data) = text::decode_text(&bytes, &config, Encoding::Ansi).ok().and_then(|t| pyjson::parse(&t).ok())
    else {
        return Ok(false);
    };
    let Some(packets) = packets(&data) else {
        return Ok(false);
    };
    let mut matched = false;
    for packet in packets {
        let Some(structure_id) = packet.get("structure") else {
            return Ok(false);
        };
        if pyjson::py_eq(structure_id, &PyValue::str(STRUCTURE_ID)) && pyjson::py_eq(packet, &entry) {
            matched = true;
            break;
        }
    }
    if !matched || !structure.is_file() {
        return Ok(false);
    }
    let installed = text::read_text(&structure, Encoding::Ansi)?;
    let library = env.library.read_text(STRUCTURE_TEMPLATE, Encoding::Ansi)?;
    Ok(installed == library)
}

/// The items of `udp.packets`: a list's items; an object or string yields keys or characters, which are not packets.
fn packets(data: &PyValue) -> Option<Vec<&PyValue>> {
    match data.get("udp")?.get("packets")? {
        PyValue::List(items) => Some(items.iter().collect()),
        PyValue::Dict(map) if map.is_empty() => Some(Vec::new()),
        PyValue::Str(text) if text.is_empty() => Some(Vec::new()),
        _ => None,
    }
}

/// Replaces the first `wrc_srs` packet by the entry (appended at the end), rewrites `config.json`
/// (indent 4) and replaces the structure file with the library copy; succeeds without `config.json`.
///
/// `Ok(false)` when parsing fails, the packet list cannot take
/// the entry (an empty object or string is not a list), or removing or writing `config.json` or copying the
/// structure fails. Failures opening `config.json` and walking the packets are errors.
pub fn patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let entry = configured_entry(env)?;
    let Some((config, structure)) = paths(env).filter(|(config, _)| config.is_file()) else {
        return Ok(true);
    };
    let bytes = fs::read(&config)?;
    let Some(mut data) = text::decode_text(&bytes, &config, Encoding::Ansi).ok().and_then(|t| pyjson::parse(&t).ok())
    else {
        return Ok(false);
    };
    let packets = match data.as_dict_mut().and_then(|d| d.get_mut("udp")).and_then(|u| u.as_dict_mut()) {
        Some(udp) => match udp.get_mut("packets") {
            Some(PyValue::List(packets)) => packets,
            Some(PyValue::Dict(map)) if map.is_empty() => return Ok(false),
            Some(PyValue::Str(text)) if text.is_empty() => return Ok(false),
            _ => return Err(PatchError::UnexpectedJson),
        },
        None => return Err(PatchError::UnexpectedJson),
    };
    let mut existing = None;
    for (index, packet) in packets.iter().enumerate() {
        let structure_id = packet.get("structure").ok_or(PatchError::UnexpectedJson)?;
        if pyjson::py_eq(structure_id, &PyValue::str(STRUCTURE_ID)) {
            existing = Some(index);
            break;
        }
    }
    if let Some(index) = existing {
        packets.remove(index);
    }
    packets.push(entry);
    if fs::remove_file(&config).is_err() {
        return Ok(false);
    }
    let chunks = pyjson::dump_chunks(&data, &DumpStyle::indented(4)).into_iter().map(|chunk| chunk.to_text());
    if text::write_chunks(&config, chunks, Encoding::Ansi, Newline::Windows).is_err() {
        return Ok(false);
    }
    let _ = fs::remove_file(&structure);
    Ok(env.library.copy_to(STRUCTURE_TEMPLATE, &structure).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::testing::{ports, prefix_fixture, scratch};

    fn native_settings(value: openrigcue_model::native_value::LegacyValue) -> crate::settings::Settings {
        let mut settings = crate::settings::Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("wrc2023", value);
        });
        settings
    }

    #[test]
    fn native_integer_port_keeps_wrc_values_without_socket_range_restriction() {
        let prefix = prefix_fixture("wrc-native-int");
        let telemetry = prefix.documents().join("My Games/WRC/telemetry");
        fs::create_dir_all(telemetry.join("udp")).unwrap();
        let config = telemetry.join("config.json");
        let library = crate::patch::library::BUILTIN;
        for (value, expected) in [
            ("+0070000".into(), "70000"),
            ("-1".into(), "-1"),
            ("\u{3000}１２_٣\u{3000}".into(), "123"),
            ("99999999999999999999999999999999999999999999".into(), "99999999999999999999999999999999999999999999"),
            (openrigcue_model::native_value::LegacyValue::Int(70000), "70000"),
            (openrigcue_model::native_value::LegacyValue::Bool(true), "1"),
            (openrigcue_model::native_value::LegacyValue::Float(-70000.9), "-70000"),
        ] {
            let settings = native_settings(value);
            let loaded = ModulePorts::at_startup(&settings);
            let env = PatchEnvironment {
                prefix: Some(&prefix),
                steam: None,
                library: &library,
                ports: &settings,
                loaded_ports: &loaded,
            };
            fs::write(&config, r#"{"udp":{"packets":[]}}"#).unwrap();
            assert!(patch(&env).unwrap(), "{expected}");
            let data = pyjson::parse(&fs::read_to_string(&config).unwrap()).unwrap();
            let PyValue::List(packets) = data.get("udp").unwrap().get("packets").unwrap() else { panic!("packets") };
            let Some(PyValue::Int(port)) = packets[0].get("port") else { panic!("int port") };
            assert_eq!(port.repr(), expected);
            assert!(patched(&env).unwrap());
        }
    }

    #[test]
    fn native_integer_port_raises_wrc_conversion_before_file_check() {
        use openrigcue_model::native_value::LegacyValue;
        let prefix = prefix_fixture("wrc-native-invalid");
        let too_long = "0".repeat(4301);
        for (value, class, message) in [
            (LegacyValue::from("bad"), "ValueError", "invalid literal for int() with base 10: 'bad'".to_owned()),
            (LegacyValue::from("1.5"), "ValueError", "invalid literal for int() with base 10: '1.5'".to_owned()),
            (LegacyValue::from(too_long), "ValueError", "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit".to_owned()),
            (LegacyValue::None, "TypeError", "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".to_owned()),
            (LegacyValue::List(vec!["123".into()]), "TypeError", "int() argument must be a string, a bytes-like object or a real number, not 'list'".to_owned()),
            (LegacyValue::Float(f64::NAN), "ValueError", "cannot convert float NaN to integer".to_owned()),
            (LegacyValue::Float(f64::INFINITY), "OverflowError", "cannot convert float infinity to integer".to_owned()),
        ] {
            let settings = native_settings(value);
            let loaded = ModulePorts::at_startup(&settings);
            let env = PatchEnvironment { prefix: Some(&prefix), steam: None, library: &crate::patch::library::BUILTIN, ports: &settings, loaded_ports: &loaded };
            for error in [patch(&env).unwrap_err(), patched(&env).unwrap_err()] {
                assert_eq!(error.native_exception(), Some((class, message.clone())));
            }
        }
        let missing = |_: PortKey| None;
        let loaded = ModulePorts::at_startup(&missing);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &missing,
            loaded_ports: &loaded,
        };
        assert!(matches!(patch(&env), Err(PatchError::MissingPort(PortKey::Wrc2023))));
        assert!(matches!(patched(&env), Err(PatchError::MissingPort(PortKey::Wrc2023))));
    }

    /// Patch / patched: existing wrc_srs packet replaced (moved last), structure copied.
    #[test]
    fn packet_and_structure() {
        let prefix = prefix_fixture("wrc");
        let telemetry = prefix.documents().join("My Games/WRC/telemetry");
        fs::create_dir_all(telemetry.join("udp")).unwrap();
        fs::write(
            telemetry.join("config.json"),
            r#"{"udp":{"packets":[{"structure":"wrc_srs","port":1},{"structure":"openmotion","port":2}]}}"#,
        )
        .unwrap();
        let dir = scratch("wrc-lib");
        fs::write(dir.join("wrc_srs.json"), "{\n\t\"id\": \"wrc_srs\"\n}").unwrap();
        let library = crate::patch::library::Library::Folder(dir);
        let lookup = ports(20782);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &library,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(installed(&env));
        assert!(!patched(&env).unwrap());
        patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(telemetry.join("config.json")).unwrap(),
            "{\r\n    \"udp\": {\r\n        \"packets\": [\r\n            {\r\n                \"structure\": \"openmotion\",\r\n                \
\"port\": 2\r\n            },\r\n            {\r\n                \"structure\": \"wrc_srs\",\r\n                \
\"packet\": \"session_update\",\r\n                \"ip\": \"127.0.0.1\",\r\n                \"port\": 20782,\r\n                \
\"frequencyHz\": -1,\r\n                \"bEnabled\": true\r\n            }\r\n        ]\r\n    }\r\n}"
        );
        assert!(patched(&env).unwrap());
        fs::write(telemetry.join("udp/wrc_srs.json"), "changed").unwrap();
        assert!(!patched(&env).unwrap());
    }

    /// Patched check: a packet without "structure" ends the check as false; patch without config.json is a
    /// no-op success.
    #[test]
    fn edge_cases() {
        let prefix = prefix_fixture("wrc-edge");
        let library = crate::patch::library::BUILTIN;
        let lookup = ports(1);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &library,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        patch(&env).unwrap();
        let telemetry = prefix.documents().join("My Games/WRC/telemetry");
        fs::create_dir_all(&telemetry).unwrap();
        fs::write(telemetry.join("config.json"), r#"{"udp":{"packets":[{"port":1}]}}"#).unwrap();
        assert!(!patched(&env).unwrap());
        // Undecodable text counts as a parse failure: false, not an error.
        fs::write(telemetry.join("config.json"), b"{\x81}").unwrap();
        assert!(!patched(&env).unwrap());
    }
}
