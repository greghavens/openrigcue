//! Console lines of the listeners: one line on standard output each.
//!
//! The listeners' log calls go to the log; the lines their error handlers print to the console do not,
//! and are written here.

#[cfg(test)]
thread_local! {
    static LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Writes one printed line.
pub(crate) fn line(text: impl AsRef<str>) {
    #[cfg(test)]
    LINES.with(|lines| lines.borrow_mut().push(text.as_ref().to_owned()));
    println!("{}", text.as_ref());
}

/// The lines printed on this thread since the last call (tests).
#[cfg(test)]
pub(crate) fn take() -> Vec<String> {
    LINES.with(|lines| std::mem::take(&mut *lines.borrow_mut()))
}

/// The printed text of a socket or file failure: `timed out` for a
/// receive timeout, `[Errno N] <text>` for a system error. An error without a system code keeps its own text.
pub(crate) fn os_error(error: &std::io::Error) -> String {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    if matches!(error.kind(), TimedOut | WouldBlock) {
        return "timed out".into();
    }
    match error.raw_os_error() {
        Some(code) => {
            let text = std::io::Error::from_raw_os_error(code).to_string();
            let text = text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text);
            format!("[Errno {code}] {text}")
        }
        None => error.to_string(),
    }
}

/// The printed form of a byte string (`b'...'` with escapes).
pub(crate) fn bytes_repr(bytes: &[u8]) -> String {
    use openrigcue_model::native_value::LegacyValue;
    openrigcue_model::python_repr::repr(&LegacyValue::Bytes(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::os_error;
    use std::io::{Error, ErrorKind};

    #[test]
    fn os_errors_read_as_python_words_them() {
        assert_eq!(os_error(&Error::from_raw_os_error(11)), "timed out", "EAGAIN of a receive timeout");
        assert_eq!(os_error(&Error::new(ErrorKind::TimedOut, "anything")), "timed out");
        assert_eq!(os_error(&Error::from_raw_os_error(98)), "[Errno 98] Address already in use");
        assert_eq!(os_error(&Error::from_raw_os_error(111)), "[Errno 111] Connection refused");
        assert_eq!(os_error(&Error::other("bind failed")), "bind failed");
    }
}
