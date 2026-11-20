//! Failures the dispatcher does not handle.
//!
//! Where a listener's liveness test or receive step fails, or the top-speed update repeats a failing
//! speed read, the failure
//! ends the dispatcher loop; it is logged and the dispatcher thread is gone until the
//! application restarts: no listener is checked or read again. A source reports such a call with
//! [`DispatcherEnds`] (after writing whatever is written before the failure), and the dispatcher
//! recognizes it with [`ends_dispatcher`].

use std::io;

use crate::derive::FailurePoint;
use crate::SourceError;

/// The dispatcher thread ends on this call; the text names the unhandled failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} (uncaught in the original dispatcher, whose thread ends here)")]
pub struct DispatcherEnds(pub String, pub FailurePoint);

impl DispatcherEnds {
    pub fn new(reason: impl Into<String>) -> Self {
        DispatcherEnds(reason.into(), FailurePoint::Other)
    }

    pub fn at_receive_data(reason: impl Into<String>) -> Self {
        Self(reason.into(), FailurePoint::ReceiveData)
    }

    pub fn at_max_speed(reason: impl Into<String>) -> Self {
        Self(reason.into(), FailurePoint::MaxSpeed)
    }

    /// The top-speed update's failed speed read, which ends the dispatcher.
    pub fn speed_read_raised() -> Self {
        Self::at_max_speed("getMaxSpeed: int(getChannel('speed')) raised")
    }
}

impl From<DispatcherEnds> for SourceError {
    fn from(error: DispatcherEnds) -> Self {
        SourceError::Io(io::Error::other(error))
    }
}

/// Whether `error` is a [`DispatcherEnds`]: the dispatcher stops for good.
pub fn ends_dispatcher(error: &SourceError) -> bool {
    failure_point(error).is_some()
}

pub fn failure_point(error: &SourceError) -> Option<FailurePoint> {
    match error {
        SourceError::Io(error) => error.get_ref()?.downcast_ref::<DispatcherEnds>().map(|error| error.1),
        SourceError::Malformed(_) => None,
    }
}

/// Test helper: the call reported [`DispatcherEnds`].
#[cfg(test)]
pub(crate) fn assert_ends<T: std::fmt::Debug>(result: Result<T, SourceError>) {
    match result {
        Err(error) if ends_dispatcher(&error) => {}
        other => panic!("expected the dispatcher to end, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognized_through_the_source_error() {
        let error: SourceError = DispatcherEnds::new("ValueError: cannot convert float NaN to integer").into();
        assert!(ends_dispatcher(&error));
        assert!(error.to_string().contains("cannot convert float NaN"));
        assert!(!ends_dispatcher(&SourceError::Malformed("x".into())));
        assert!(!ends_dispatcher(&SourceError::Io(io::Error::other("timed out"))));
    }
}
