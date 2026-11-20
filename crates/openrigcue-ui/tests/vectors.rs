//! The slider arithmetic against the test vectors of `tests/vectors.json`.

use openrigcue_ui::slider::{py_round, RangeScale, SliderScale};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors.json")).unwrap()
}

fn scale_of(case: &Value) -> [f64; 3] {
    let s = case["scale"].as_array().unwrap();
    [s[0].as_f64().unwrap(), s[1].as_f64().unwrap(), s[2].as_f64().unwrap()]
}

/// A slider's value for a pointer fraction, and for a stored value set on it.
#[test]
fn sliders_match_cpython() {
    for case in vectors()["sliders"].as_array().unwrap() {
        let [min, max, step] = scale_of(case);
        let scale = SliderScale::new(min, max, step);
        let got = match case.get("fraction") {
            Some(f) => scale.value_at(f.as_f64().unwrap()),
            None => scale.snap(case["stored"].as_f64().unwrap()),
        };
        assert_eq!(got, case["value"].as_f64().unwrap(), "{case}");
    }
}

/// A range slider's values for its pointer fractions, and for stored values set on it.
#[test]
fn ranges_match_cpython() {
    for case in vectors()["ranges"].as_array().unwrap() {
        let [min, max, step] = scale_of(case);
        let scale = RangeScale::new(min, max, step);
        let got = match case.get("fractions") {
            Some(f) => scale.values_at(f[0].as_f64().unwrap(), f[1].as_f64().unwrap()),
            None => scale.snap(case["stored"][0].as_f64().unwrap(), case["stored"][1].as_f64().unwrap()),
        };
        let want = (case["value"][0].as_f64().unwrap(), case["value"][1].as_f64().unwrap());
        assert_eq!(got, want, "{case}");
    }
}

/// Rounding to the nearest integer, halves to even.
#[test]
fn round_matches_cpython() {
    for case in vectors()["rounding"].as_array().unwrap().iter().filter(|c| c.get("round0").is_some()) {
        assert_eq!(py_round(case["x"].as_f64().unwrap()), case["round0"].as_f64().unwrap(), "{case}");
    }
}
