//! The Unicode 15.1 alphanumeric character class, independent of Rust's Unicode version.

include!("python_alnum_ranges.rs");

pub fn is_alphanumeric(value: char) -> bool {
    let value = u32::from(value);
    let after = ALNUM_RANGES.partition_point(|(start, _)| *start <= value);
    after != 0 && value <= ALNUM_RANGES[after - 1].1
}

pub fn str_is_alphanumeric(value: &str) -> bool {
    !value.is_empty() && value.chars().all(is_alphanumeric)
}
