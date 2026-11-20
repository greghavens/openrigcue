//! What the dispatcher needs from a game listener, and the adapter over [`crate::Source`].

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use crate::{NativeRead, Source};

/// Rate channels the dispatcher requests that are not part of the channel registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    /// `yaw_rate`: front/rear slip angles (DAMS mode 2) and the yaw of flying games.
    Yaw,
    /// `pitch_rate`: pitch when the profile selects rate instead of angle.
    Pitch,
    /// `roll_rate`: roll when the profile selects rate instead of angle.
    Roll,
}

/// Where an unhandled listener failure leaves the dispatcher's current pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePoint {
    CheckStatus,
    ReceiveData,
    MaxSpeed,
    /// A raw-value/getter failure whose exact processing point is not represented yet.
    Other,
}

/// One game listener as the dispatcher drives it: a liveness test, a receive step and channel reads.
///
/// A channel the listener cannot provide is `None`;
/// the dispatcher's handling of the failed read decides what is written.
pub trait Listener: Send {
    /// Listener identifier (for example `"dirt"`).
    fn id(&self) -> &str;
    /// Whether the game is currently delivering data.
    fn is_running(&mut self) -> bool;
    /// The last liveness test failed with an unhandled error. The dispatcher
    /// must stop before scanning another listener or publishing a completed tick.
    fn check_status_raised(&self) -> bool {
        self.failure_point() == Some(FailurePoint::CheckStatus)
    }
    fn failure_point(&self) -> Option<FailurePoint> {
        None
    }
    /// Takes in the newest data.
    fn receive(&mut self);
    /// Numeric compatibility view. Typed consumers first use read_native to retain values and failed reads.
    fn channel(&self, name: &str) -> Option<f64>;
    /// Typed answer, retaining the int/float/text/none distinctions.
    fn native_channel(&self, _name: &str) -> Option<LegacyValue> {
        None
    }
    fn read_native(&self, name: &str) -> NativeRead {
        self.native_channel(name).map_or(NativeRead::Unsupported, NativeRead::Value)
    }
    /// Value of a rate channel outside the registry (`yaw_rate`, `pitch_rate`, `roll_rate`).
    fn rate(&self, rate: Rate) -> Option<f64>;
    /// Vehicle identifier (the `car_name` channel).
    fn vehicle(&self) -> Option<String>;
    /// Track identifier (the `track_loc` channel).
    fn location(&self) -> Option<String>;
}

/// Adapts a [`Source`] to the dispatcher's [`Listener`] contract.
///
/// The source writes into a private frame whose channels start with a distinct NaN payload; a channel the source
/// has never written is unavailable, while a source's ordinary NaN remains a value. Rate channels are not part of the
/// [`Source`] contract and are always unavailable.
pub struct SourceListener<S: Source> {
    source: S,
    frame: Telemetry,
    failure: Option<FailurePoint>,
}

const UNWRITTEN: u64 = 0x7ff8_0000_4f52_4355;

impl<S: Source> SourceListener<S> {
    /// Wraps a source; nothing is read until the dispatcher selects it.
    pub fn new(source: S) -> Self {
        let mut frame = Telemetry::off();
        for name in Telemetry::CHANNELS {
            if let Some(value) = frame.get_mut(name) {
                *value = f64::from_bits(UNWRITTEN);
            }
        }
        SourceListener { source, frame, failure: None }
    }
}

impl<S: Source> Listener for SourceListener<S> {
    fn id(&self) -> &str {
        self.source.id()
    }

    fn is_running(&mut self) -> bool {
        self.failure = None;
        match self.source.is_running() {
            Ok(running) => running,
            Err(error) => {
                if crate::udp_racing::unhandled::ends_dispatcher(&error) {
                    self.failure = Some(FailurePoint::CheckStatus);
                }
                tracing::warn!(listener = self.source.id(), %error, "liveness check failed");
                false
            }
        }
    }

    fn receive(&mut self) {
        self.failure = None;
        if let Err(error) = self.source.receive_native(&mut self.frame) {
            self.failure = crate::udp_racing::unhandled::failure_point(&error);
            tracing::warn!(listener = self.source.id(), %error, "reading telemetry failed");
        }
    }

    fn channel(&self, name: &str) -> Option<f64> {
        if self.source.channel_read_raised(name) {
            return None;
        }
        self.frame.get(name).filter(|value| value.to_bits() != UNWRITTEN)
    }

    fn failure_point(&self) -> Option<FailurePoint> {
        self.failure
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.source.read_native(name) {
            NativeRead::Value(value) => Some(value),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    fn read_native(&self, name: &str) -> NativeRead {
        self.source.read_native(name)
    }

    fn rate(&self, rate: Rate) -> Option<f64> {
        let name = match rate {
            Rate::Yaw => "yaw_rate",
            Rate::Pitch => "pitch_rate",
            Rate::Roll => "roll_rate",
        };
        match self.source.read_native(name) {
            NativeRead::Value(value) => value.number(),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    fn vehicle(&self) -> Option<String> {
        match self.source.read_native("car_name") {
            NativeRead::Raised => None,
            NativeRead::Value(LegacyValue::Str(value)) => Some(value),
            NativeRead::Value(LegacyValue::Int(0)) => Some(String::new()),
            _ => Some(self.frame.vehicle.clone()),
        }
    }

    fn location(&self) -> Option<String> {
        match self.source.read_native("track_loc") {
            NativeRead::Raised => None,
            NativeRead::Value(LegacyValue::Str(value)) => Some(value),
            NativeRead::Value(LegacyValue::Int(0)) => Some(String::new()),
            _ => Some(self.frame.location.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceError;

    #[test]
    fn explicit_native_read_distinguishes_raise_none_and_numeric_compatibility() {
        let source = super::super::testing::ExplicitReadSource::default();
        let reads = source.reads.clone();
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.channel("speed"), Some(20.0), "older numeric frame is readable");
        assert_eq!(listener.read_native("speed"), NativeRead::Raised);
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::None));
        assert_eq!(listener.read_native("gear"), NativeRead::Unsupported);
        assert_eq!(listener.vehicle(), None);
        assert_eq!(listener.location(), None);
        assert_eq!(listener.rate(Rate::Yaw), None);
        assert_eq!(*reads.lock().unwrap(), ["speed", "rpm", "gear", "car_name", "track_loc", "yaw_rate"]);
    }

    #[test]
    fn failed_identity_getters_hide_already_captured_names_and_recover() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct IdentitySource(Arc<AtomicBool>);
        impl Source for IdentitySource {
            fn id(&self) -> &'static str {
                "identity"
            }
            fn is_running(&mut self) -> Result<bool, SourceError> {
                Ok(true)
            }
            fn poll(&mut self, frame: &mut Telemetry) -> Result<bool, SourceError> {
                frame.vehicle = "car".into();
                frame.location = "track".into();
                frame.speed = 20.0;
                Ok(true)
            }
            fn channel_read_raised(&self, name: &str) -> bool {
                matches!(name, "car_name" | "track_loc") && self.0.load(Ordering::Relaxed)
            }
        }
        let raised = Arc::new(AtomicBool::new(false));
        let mut listener = SourceListener::new(IdentitySource(raised.clone()));
        listener.receive();
        assert_eq!(listener.vehicle().as_deref(), Some("car"));
        assert_eq!(listener.location().as_deref(), Some("track"));
        raised.store(true, Ordering::Relaxed);
        assert_eq!(listener.vehicle(), None);
        assert_eq!(listener.location(), None);
        assert_eq!(listener.channel("speed"), Some(20.0));
        raised.store(false, Ordering::Relaxed);
        assert_eq!(listener.vehicle().as_deref(), Some("car"));
        assert_eq!(listener.location().as_deref(), Some("track"));
    }

    struct BoundarySource {
        check_failure: bool,
        poll_failure: Option<FailurePoint>,
    }

    impl Source for BoundarySource {
        fn id(&self) -> &'static str {
            "boundary"
        }
        fn is_running(&mut self) -> Result<bool, SourceError> {
            if self.check_failure {
                Err(crate::udp_racing::unhandled::DispatcherEnds::new("liveness raised").into())
            } else {
                Ok(true)
            }
        }
        fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
            telemetry.rpm = f64::NAN;
            match self.poll_failure {
                Some(point) => Err(crate::udp_racing::unhandled::DispatcherEnds("read raised".into(), point).into()),
                None => Ok(true),
            }
        }
    }

    #[test]
    fn native_nan_is_a_value_and_unwritten_channels_are_absent() {
        let mut listener = SourceListener::new(BoundarySource { check_failure: false, poll_failure: None });
        assert_eq!(listener.channel("rpm"), None);
        listener.receive();
        assert!(listener.channel("rpm").unwrap().is_nan());
        assert_eq!(listener.channel("gear"), None);
    }

    #[test]
    fn source_adapter_retains_fatal_boundaries_and_prior_writes() {
        let mut listener = SourceListener::new(BoundarySource { check_failure: true, poll_failure: None });
        assert!(!listener.is_running());
        assert!(listener.check_status_raised());
        assert_eq!(listener.failure_point(), Some(FailurePoint::CheckStatus));
        for point in [FailurePoint::ReceiveData, FailurePoint::MaxSpeed] {
            let mut listener = SourceListener::new(BoundarySource { check_failure: false, poll_failure: Some(point) });
            assert!(listener.is_running());
            listener.receive();
            assert_eq!(listener.failure_point(), Some(point));
            assert!(listener.channel("rpm").unwrap().is_nan(), "writes before the error remain");
        }
    }

    struct FakeSource {
        running: bool,
    }

    impl Source for FakeSource {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn is_running(&mut self) -> Result<bool, SourceError> {
            if self.running {
                Ok(true)
            } else {
                Err(SourceError::Malformed("down".into()))
            }
        }
        fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
            telemetry.speed = 12.0;
            telemetry.vehicle = "car".into();
            Ok(true)
        }
    }

    /// Channels the listener does not provide: unwritten channels are unavailable.
    #[test]
    fn unwritten_channels_are_unavailable() {
        let mut listener = SourceListener::new(FakeSource { running: true });
        assert!(listener.is_running());
        assert_eq!(listener.channel("speed"), None);
        listener.receive();
        assert_eq!(listener.channel("speed"), Some(12.0));
        assert_eq!(listener.channel("rpm"), None);
        assert_eq!(listener.rate(Rate::Yaw), None);
        assert_eq!(listener.vehicle().as_deref(), Some("car"));
    }

    /// A failing liveness check counts as not running.
    #[test]
    fn liveness_error_is_not_running() {
        let mut listener = SourceListener::new(FakeSource { running: false });
        assert!(!listener.is_running());
    }
}
