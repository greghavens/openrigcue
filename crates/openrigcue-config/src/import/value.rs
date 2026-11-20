//! Dynamically typed values read from imported Sim Racing Studio files.
//!
//! `config.ini` is an INI file of sections holding strings and string lists, `profile.db` and
//! `metadata.db` are SQLite files whose values are Python pickles of dicts, and `.srs` profiles are
//! JSON. [`LegacyValue`] models the value kinds those files can hold, keeping the distinctions
//! (int vs float, insertion-ordered dicts) that the migrations depend on.

pub use openrigcue_model::native_value::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_strings_keep_python_grammar_and_i64_minimum() {
        assert_eq!(parse_py_int("-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse_py_int("9223372036854775808"), None);
        assert_eq!(parse_py_int("１２_٣"), Some(123));
        assert_eq!(parse_py_float("١_２.٣e+２"), Some(1230.0));
        for text in ["1_e2", "1e_2", "1._2", "1_.2", "+_1", "n_an", "1\u{1c}"] {
            assert_eq!(parse_py_float(text), None, "{text:?}");
        }
    }

    #[test]
    #[ignore = "requires python3 for an independent numeric grammar reference"]
    fn numeric_strings_match_cpython_reference() {
        let code = r#"
import json,struct
samples=['', '+', '-', '1.0', '-0', '-nan', 'NaN', '+INFINITY', '1e309', '1e-4000',
 '1_e2', '1e_2', '1._2', '1_.2', '+_1', '1__2', '_1', '1_', '0x10', '0b10',
 '-9223372036854775808', '9223372036854775808', '1_2.3_4e+5_6', '١_２.٣e+２']
for space in [' ', '\t', '\n', '\r', '\v', '\f', '\x1c', '\x1f', '\x85', '\xa0', '\u2003', '\u3000']:
 samples += [space+'12'+space, space+'1.25'+space]
for digit in ['0', '１', '٢', '३', '𝟝', '²', '₂']:
 for suffix in ['', '_1', '._1', '.1', 'e1', 'e_1', 'e+1', '__1']:
  samples.append(digit+suffix)
rows=[]
for sample in samples:
 try:
  n=int(sample)
  integer=n if -2**63 <= n < 2**63 else None
 except ValueError: integer=None
 try: bits=struct.pack('>d',float(sample)).hex()
 except ValueError: bits=None
 rows.append([sample,integer,bits])
print(json.dumps(rows))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(code).output().unwrap();
        assert!(output.status.success());
        let rows: Vec<(String, Option<i64>, Option<String>)> = serde_json::from_slice(&output.stdout).unwrap();
        for (text, integer, float) in rows {
            assert_eq!(parse_py_int(&text), integer, "int({text:?})");
            assert_eq!(parse_py_float(&text).map(|v| format!("{:016x}", v.to_bits())), float, "float({text:?})");
        }
    }

    #[test]
    fn dict_keeps_python_insertion_semantics() {
        let mut d = Dict::new();
        d.insert("a", 1.into());
        d.insert("b", 2.into());
        d.insert("a", 3.into());
        assert_eq!(d.str_keys(), ["a", "b"]);
        assert_eq!(d.get("a"), Some(&LegacyValue::Int(3)));
        d.remove("a");
        d.insert("a", 4.into());
        assert_eq!(d.str_keys(), ["b", "a"]);
    }

    /// The accepted true/false spellings, case-insensitive.
    #[test]
    fn str2bool_spellings() {
        for t in ["yes", "TRUE", "t", "Y", "1"] {
            assert_eq!(str2bool(t), Some(true), "{t}");
        }
        for t in ["no", "False", "f", "N", "0"] {
            assert_eq!(str2bool(t), Some(false), "{t}");
        }
        assert_eq!(str2bool("2"), None);
        assert_eq!(str2bool(""), None);
    }

    #[test]
    fn python_int_and_float_parsing() {
        assert_eq!(parse_py_int(" 27 "), Some(27));
        assert_eq!(parse_py_int("-1"), Some(-1));
        assert_eq!(parse_py_int("1_000"), Some(1000));
        assert_eq!(parse_py_int("15.0"), None);
        assert_eq!(parse_py_int(""), None);
        assert_eq!(parse_py_float("1.66"), Some(1.66));
        assert_eq!(parse_py_float(" 2 "), Some(2.0));
        assert_eq!(parse_py_float("abc"), None);
        assert_eq!(LegacyValue::Float(-2.7).py_int(), Some(-2));
    }

    #[test]
    fn python_float_repr() {
        assert_eq!(py_float_repr(15.0), "15.0");
        assert_eq!(py_float_repr(1.66), "1.66");
        assert_eq!(py_float_repr(0.01), "0.01");
        assert_eq!(py_float_repr(0.0001), "0.0001");
        assert_eq!(py_float_repr(0.00001), "1e-05");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(123456789012345.0), "123456789012345.0");
        assert_eq!(py_float_repr(-0.5), "-0.5");
        assert_eq!(py_float_repr(1.6929), "1.6929");
    }
}
