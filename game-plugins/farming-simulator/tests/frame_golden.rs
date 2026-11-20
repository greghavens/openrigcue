//! `format_fixed4` against Lua 5.1 `string.format("%.4f")`.
//!
//! Every line of `tests/golden/format_vectors.tsv` holds a value and the
//! text Lua 5.1 formats it to; `format_fixed4` must give the same text.

use std::path::Path;

use openrigcue_fs_mods::frame::format_fixed4;

#[test]
fn format_matches_lua_string_format_vectors() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/format_vectors.tsv");
    let text = std::fs::read_to_string(path).unwrap();
    let mut n = 0;
    for line in text.lines() {
        let (v, expected) = line.split_once('\t').unwrap();
        let v: f64 = v.parse().unwrap();
        assert_eq!(format_fixed4(v), expected, "value {v:e}");
        n += 1;
    }
    assert!(n > 3000);
}
