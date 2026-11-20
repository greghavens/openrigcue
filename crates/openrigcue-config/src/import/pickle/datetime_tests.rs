use super::{decode, encode, DateTimeParts, LegacyValue};

fn construct(args: Vec<LegacyValue>) -> Result<LegacyValue, super::PickleError> {
    let mut bytes = b"\x80\x04cdatetime\ndatetime\n".to_vec();
    let tuple = encode(&LegacyValue::Tuple(args));
    bytes.extend_from_slice(&tuple[2..tuple.len() - 1]);
    bytes.extend(b"R.");
    decode(&bytes)
}

#[test]
fn datetime_reduction_accepts_calendar_arguments_and_latin1_state() {
    let parts =
        DateTimeParts { year: 2024, month: 2, day: 29, hour: 0, minute: 0, second: 0, microsecond: 0, fold: false };
    assert_eq!(construct(vec![2024.into(), 2.into(), 29.into()]), Ok(LegacyValue::DateTime(parts)));
    let state = [7_u8, 232, 130, 29, 23, 59, 58, 15, 66, 63];
    let text: String = state.iter().copied().map(char::from).collect();
    let expected = DateTimeParts { hour: 23, minute: 59, second: 58, microsecond: 999999, fold: true, ..parts };
    assert_eq!(construct(vec![text.into(), LegacyValue::None]), Ok(LegacyValue::DateTime(expected)));
}

#[test]
fn datetime_calendar_arguments_keep_validation_order() {
    for (values, message) in [
        (vec![0, 13, 0], "year 0 is out of range"),
        (vec![2024, 13, 0], "month must be in 1..12"),
        (vec![2023, 2, 29], "day is out of range for month"),
        (vec![2024, 2, 29, 24, 60], "hour must be in 0..23"),
        (vec![2024, 2, 29, 23, 60], "minute must be in 0..59"),
        (vec![2024, 2, 29, 23, 59, 60], "second must be in 0..59"),
        (vec![2024, 2, 29, 23, 59, 59, 1000000], "microsecond must be in 0..999999"),
    ] {
        assert_eq!(
            construct(values.into_iter().map(LegacyValue::Int).collect()).unwrap_err().native_exception(),
            Some(("ValueError", message.into()))
        );
    }
}
