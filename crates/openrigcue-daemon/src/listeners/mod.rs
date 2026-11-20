//! The registered game listeners and the adapter the dispatcher drives them
//! through.
//!
//! The dispatcher needs more from a listener than [`openrigcue_sources::Source`] offers: the rate channels
//! `yaw_rate`/`pitch_rate`/`roll_rate`, the error texts some listeners add to the game errors, the API
//! listener's fan override, and runtime values the listeners read on every call
//! (the running game, `pause_telemetry_when_not_playing`, the open window titles, the joystick switch).
//! [`GameSource`] adds those per source type and [`GameListener`] adapts it to the dispatcher's
//! [`Listener`] contract.

mod registry;
mod sources;

use std::collections::BTreeSet;

use openrigcue_model::Telemetry;
use openrigcue_sources::api::{JoystickOptions, JoystickProfile};
use openrigcue_sources::derive::{FailurePoint, Listener, Rate};
use openrigcue_sources::Source;

use crate::runtime::Shared;

pub use registry::{
    build_listeners, build_listeners_with_model_reader, build_listeners_with_native_readers, console_reader,
    elite_pid_reader, forwarding_reader, import_listeners, iracing_dams_reader, joystick_profile, motion_model_reader,
    network_ports_reader, pause_reader, windows_reader, xplane_lateral_reader, ListenerInputs,
};

/// Runtime values supplied before a tick; game/listener selection also updates at its assignments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceContext {
    /// The running listener, `None` without one.
    pub listener_running: Option<String>,
    /// The running game, `None` in stand-by.
    pub game_running: Option<String>,
    /// `APP.pause_telemetry_when_not_playing`.
    pub pause_when_not_playing: bool,
    /// The open window titles: `None` until the first window snapshot.
    pub window_titles: Option<Vec<String>>,
    /// `CUSTOM_FEATURES.dams_front_tl_mode` as an integer, `None` when it is not one.
    pub dams_front_tl_mode: Option<i64>,
    /// `CUSTOM_FEATURES.rollback_xplane_latg`.
    pub rollback_xplane_lateral_g: bool,
    /// Whether the `joy` listener is switched on.
    pub joystick_enabled: bool,
    /// The selected joystick profile.
    pub joystick_profile: JoystickProfile,
    /// The stored profile and its name. The inner None is a missing database key; the outer None is for
    /// callers using the typed profile above.
    pub joystick_raw_profile: Option<(String, Option<openrigcue_config::import::value::LegacyValue>)>,
    pub joystick_options: JoystickOptions,
    /// `APP.playstation_ip`.
    pub playstation_ip: String,
    /// Whether the tune's `motion_effects.pitch_type` is `angle` (a missing key reads as `angle`).
    pub pitch_angle: bool,
    /// Whether the tune's `motion_effects.roll_type` is `angle` (a missing key reads as `angle`).
    pub roll_angle: bool,
}

/// What listeners report back besides telemetry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceOutputs {
    /// What the listeners did to the game errors since the telemetry loop last published them.
    pub game_errors: ErrorChanges,
    /// Which listeners had errors at their last call, to see one clear them.
    with_errors: BTreeSet<String>,
    /// The API listener's last datagram drives the fans directly.
    pub fan_override: bool,
    /// A listener failed with an error the dispatcher does not handle
    /// ([`openrigcue_sources::udp_racing::unhandled::ends_dispatcher`]): the dispatcher thread ends with it.
    pub dispatcher_ended: Option<String>,
}

/// Changes to the shared game error set, in the order the listeners make them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ErrorChanges {
    /// A listener emptied the set before the texts below were added.
    pub reset: bool,
    /// The error texts added.
    pub added: BTreeSet<String>,
}

impl ErrorChanges {
    /// Applies the changes to `errors`.
    pub fn apply_to(&self, errors: &mut BTreeSet<String>) {
        if self.reset {
            errors.clear();
        }
        errors.extend(self.added.iter().cloned());
    }
}

impl SourceOutputs {
    /// One listener call's errors: a listener that has errors adds them (the text is added on every
    /// failing call); one whose errors went away emptied the shared set.
    pub fn record_errors(&mut self, id: &str, errors: &BTreeSet<String>) {
        if errors.is_empty() {
            if self.with_errors.remove(id) {
                self.game_errors = ErrorChanges { reset: true, added: BTreeSet::new() };
            }
        } else {
            self.with_errors.insert(id.to_owned());
            self.game_errors.added.extend(errors.iter().cloned());
        }
    }

    /// Takes the changes made since the last call.
    pub fn take_error_changes(&mut self) -> ErrorChanges {
        std::mem::take(&mut self.game_errors)
    }
}

/// The non-registry rate channels a listener answers (`yaw_rate`, `pitch_rate`, `roll_rate`);
/// `None` where the listener has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rates {
    pub yaw: Option<f64>,
    pub pitch: Option<f64>,
    pub roll: Option<f64>,
}

impl Rates {
    /// The answer of a listener without rate branches: 0 for every rate.
    pub const ZERO: Rates = Rates { yaw: Some(0.0), pitch: Some(0.0), roll: Some(0.0) };

    fn get(self, rate: Rate) -> Option<f64> {
        match rate {
            Rate::Yaw => self.yaw,
            Rate::Pitch => self.pitch,
            Rate::Roll => self.roll,
        }
    }
}

/// A telemetry source as the service drives it.
pub trait GameSource: Source {
    /// The rate channels as of the last poll. The default is the answer of a listener without rate
    /// branches: 0 for every rate.
    fn rates(&self) -> Rates {
        Rates::ZERO
    }

    /// Takes the runtime values the listener reads before its liveness check.
    fn prepare(&mut self, _context: &SourceContext) {}

    /// The listener's game error texts, for the listeners that add any.
    fn game_errors(&self) -> Option<&BTreeSet<String>> {
        None
    }

    /// Whether the last datagram asked for direct fan control (the API listener's `lla` packet).
    fn fan_override(&self) -> bool {
        false
    }
}

/// Adapts a [`GameSource`] to the dispatcher's [`Listener`] contract.
///
/// The source writes into a private frame whose channels start as [`UNWRITTEN`]; a channel the source has never
/// written is unavailable, every other keeps the source's last answer,
/// NaN included.
/// The marker of a channel no source wrote: a NaN with a payload no arithmetic produces (sources write the
/// canonical NaN).
const UNWRITTEN: u64 = 0x7ff8_0000_4f52_4355;

pub struct GameListener<S: GameSource> {
    source: S,
    frame: Telemetry,
    context: Shared<SourceContext>,
    outputs: Shared<SourceOutputs>,
    check_status_raised: bool,
    poll_failure: Option<FailurePoint>,
}

impl<S: GameSource> GameListener<S> {
    pub fn new(source: S, context: Shared<SourceContext>, outputs: Shared<SourceOutputs>) -> Self {
        let mut frame = Telemetry::off();
        for name in Telemetry::CHANNELS {
            if let Some(value) = frame.get_mut(name) {
                *value = f64::from_bits(UNWRITTEN);
            }
        }
        GameListener { source, frame, context, outputs, check_status_raised: false, poll_failure: None }
    }

    /// An error the dispatcher does not handle ends the dispatcher thread.
    fn check_end(&self, error: &openrigcue_sources::SourceError) -> bool {
        if openrigcue_sources::udp_racing::unhandled::ends_dispatcher(error) {
            self.outputs.write().dispatcher_ended = Some(format!("{}: {error}", self.source.id()));
            true
        } else {
            false
        }
    }

    fn publish(&self) {
        let mut outputs = self.outputs.write();
        let id = self.source.id();
        let none = BTreeSet::new();
        outputs.record_errors(id, self.source.game_errors().unwrap_or(&none));
        if id == "api" {
            outputs.fan_override = self.source.fan_override();
        }
    }
}

impl<S: GameSource> Listener for GameListener<S> {
    fn id(&self) -> &str {
        self.source.id()
    }

    /// The liveness check, after the source has read the runtime values it needs.
    fn is_running(&mut self) -> bool {
        let context = self.context.snapshot();
        self.source.prepare(&context);
        let running = match self.source.is_running() {
            Ok(running) => running,
            Err(error) => {
                self.check_status_raised = self.check_end(&error);
                tracing::warn!(listener = self.source.id(), %error, "liveness check failed");
                false
            }
        };
        self.publish();
        running
    }

    fn check_status_raised(&self) -> bool {
        self.check_status_raised
    }

    fn failure_point(&self) -> Option<FailurePoint> {
        self.poll_failure
    }

    /// Receives the pending data into the frame.
    fn receive(&mut self) {
        if let Err(error) = self.source.receive_native(&mut self.frame) {
            self.poll_failure = openrigcue_sources::udp_racing::unhandled::failure_point(&error);
            self.check_end(&error);
            tracing::warn!(listener = self.source.id(), %error, "reading telemetry failed");
        }
        self.publish();
    }

    fn channel(&self, name: &str) -> Option<f64> {
        if self.source.channel_read_raised(name) {
            return None;
        }
        self.frame.get(name).filter(|value| value.to_bits() != UNWRITTEN)
    }

    fn native_channel(&self, name: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
        match self.source.read_native(name) {
            openrigcue_sources::NativeRead::Value(value) => Some(value),
            openrigcue_sources::NativeRead::Raised | openrigcue_sources::NativeRead::Unsupported => None,
        }
    }

    fn read_native(&self, name: &str) -> openrigcue_sources::NativeRead {
        self.source.read_native(name)
    }

    fn rate(&self, rate: Rate) -> Option<f64> {
        let name = match rate {
            Rate::Yaw => "yaw_rate",
            Rate::Pitch => "pitch_rate",
            Rate::Roll => "roll_rate",
        };
        match self.source.read_native(name) {
            openrigcue_sources::NativeRead::Value(value) => value.number(),
            openrigcue_sources::NativeRead::Raised => None,
            openrigcue_sources::NativeRead::Unsupported => self.source.rates().get(rate),
        }
    }

    fn vehicle(&self) -> Option<String> {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::NativeRead;
        match self.source.read_native("car_name") {
            NativeRead::Raised => None,
            NativeRead::Value(LegacyValue::Str(value)) => Some(value),
            NativeRead::Value(LegacyValue::Int(0)) => Some(String::new()),
            _ => Some(self.frame.vehicle.clone()),
        }
    }

    fn location(&self) -> Option<String> {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::NativeRead;
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
    use openrigcue_sources::SourceError;

    #[test]
    fn explicit_native_reads_are_forwarded_once_without_cached_fallbacks() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::NativeRead;
        struct Explicit(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl Source for Explicit {
            fn id(&self) -> &'static str {
                "forza"
            }
            fn is_running(&mut self) -> Result<bool, SourceError> {
                Ok(true)
            }
            fn poll(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
                panic!("native receive must not replay the numeric compatibility poll")
            }
            fn receive_native(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
                telemetry.speed = 20.0;
                telemetry.vehicle = "older car".into();
                telemetry.location = "older track".into();
                Ok(true)
            }
            fn native_channel(&self, _: &str) -> Option<LegacyValue> {
                panic!("legacy probe")
            }
            fn read_native(&self, name: &str) -> NativeRead {
                self.0.lock().unwrap().push(name.into());
                match name {
                    "speed" | "yaw_rate" | "car_name" | "track_loc" => NativeRead::Raised,
                    "rpm" => NativeRead::Value(LegacyValue::None),
                    _ => NativeRead::Unsupported,
                }
            }
        }
        impl GameSource for Explicit {}
        let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut listener = GameListener::new(Explicit(reads.clone()), Shared::default(), Shared::default());
        listener.receive();
        assert_eq!(listener.read_native("speed"), NativeRead::Raised);
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::None));
        assert_eq!(listener.rate(Rate::Yaw), None);
        assert_eq!(listener.vehicle(), None);
        assert_eq!(listener.location(), None);
        assert_eq!(*reads.lock().unwrap(), ["speed", "rpm", "yaw_rate", "car_name", "track_loc"]);
    }

    #[derive(Default)]
    struct Fake {
        running: bool,
        fatal_check: bool,
        poll_failure: Option<FailurePoint>,
        getters_raised: bool,
        native_speed: Option<openrigcue_model::native_value::LegacyValue>,
        prepared_game: Option<String>,
        errors: BTreeSet<String>,
    }

    impl Source for Fake {
        fn id(&self) -> &'static str {
            "forza"
        }
        fn is_running(&mut self) -> Result<bool, SourceError> {
            if self.fatal_check {
                return Err(openrigcue_sources::udp_racing::unhandled::DispatcherEnds::new("fatal checkStatus").into());
            }
            Ok(self.running)
        }
        fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
            telemetry.speed = 20.0;
            telemetry.vehicle = "car".into();
            if let Some(point) = self.poll_failure {
                let error = openrigcue_sources::udp_racing::unhandled::DispatcherEnds("fatal poll".to_owned(), point);
                return Err(error.into());
            }
            Ok(true)
        }
        fn channel_read_raised(&self, name: &str) -> bool {
            matches!(name, "speed" | "car_name" | "track_loc" | "yaw_rate" | "pitch_rate" | "roll_rate")
                && self.getters_raised
        }
        fn native_channel(&self, name: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
            if name == "speed" {
                self.native_speed.clone()
            } else {
                None
            }
        }
    }

    impl GameSource for Fake {
        fn prepare(&mut self, context: &SourceContext) {
            self.prepared_game.clone_from(&context.game_running);
        }
        fn game_errors(&self) -> Option<&BTreeSet<String>> {
            Some(&self.errors)
        }
    }

    #[test]
    fn uncaught_check_status_is_reported_to_the_dispatcher_at_the_call() {
        let outputs: Shared<SourceOutputs> = Shared::default();
        let mut listener =
            GameListener::new(Fake { fatal_check: true, ..Fake::default() }, Shared::default(), outputs.clone());
        assert!(!listener.is_running());
        assert!(listener.check_status_raised());
        assert!(outputs.read().dispatcher_ended.as_ref().unwrap().contains("fatal checkStatus"));
        assert_eq!(listener.channel("speed"), None);
    }

    #[test]
    fn poll_exception_retains_its_original_processing_point() {
        for point in [FailurePoint::ReceiveData, FailurePoint::MaxSpeed, FailurePoint::Other] {
            let outputs: Shared<SourceOutputs> = Shared::default();
            let mut listener = GameListener::new(
                Fake { running: true, poll_failure: Some(point), ..Fake::default() },
                Shared::default(),
                outputs.clone(),
            );
            assert!(listener.is_running());
            listener.receive();
            assert_eq!(listener.failure_point(), Some(point));
            assert!(outputs.read().dispatcher_ended.is_some());
        }
    }

    /// The channel contract: a channel the listener never answered is unavailable, an answered
    /// one keeps its last value; a listener without rate branches answers 0.
    #[test]
    fn raised_channel_read_hides_earlier_answers_at_the_daemon_adapter() {
        use openrigcue_model::native_value::LegacyValue;
        let mut listener = GameListener::new(
            Fake { running: true, native_speed: Some(LegacyValue::Str("20".into())), ..Fake::default() },
            Shared::default(),
            Shared::default(),
        );
        listener.receive();
        assert_eq!(listener.channel("speed"), Some(20.0));
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Str("20".into())));
        assert_eq!(listener.vehicle().as_deref(), Some("car"));
        assert_eq!(listener.location().as_deref(), Some(""));
        listener.source.getters_raised = true;
        assert_eq!(listener.channel("speed"), None);
        assert_eq!(listener.native_channel("speed"), None);
        assert_eq!(listener.vehicle(), None);
        assert_eq!(listener.location(), None);
        for rate in [Rate::Yaw, Rate::Pitch, Rate::Roll] {
            assert_eq!(listener.rate(rate), None);
        }
        listener.source.getters_raised = false;
        assert_eq!(listener.channel("speed"), Some(20.0));
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Str("20".into())));
        assert_eq!(listener.vehicle().as_deref(), Some("car"));
        assert_eq!(listener.location().as_deref(), Some(""));
        for rate in [Rate::Yaw, Rate::Pitch, Rate::Roll] {
            assert_eq!(listener.rate(rate), Some(0.0));
        }
    }

    #[test]
    fn get_channel_answers() {
        let (context, outputs) = (Shared::default(), Shared::default());
        let mut listener = GameListener::new(Fake { running: true, ..Fake::default() }, context, outputs);
        assert!(listener.is_running());
        assert_eq!(listener.channel("speed"), None);
        listener.receive();
        assert_eq!(listener.channel("speed"), Some(20.0));
        assert_eq!(listener.channel("rpm"), None);
        assert_eq!(listener.rate(Rate::Yaw), Some(0.0));
        assert_eq!(listener.vehicle().as_deref(), Some("car"));
    }

    /// The liveness check reads the runtime values at call time: the context is handed over before every check; a
    /// listener's errors are added to the game errors at every call that has them, and a listener whose errors went away empties
    /// the set.
    #[test]
    fn check_status_reads_runtime_values_and_publishes_errors() {
        let context = Shared::new(SourceContext { game_running: Some("acc".into()), ..SourceContext::default() });
        let outputs: Shared<SourceOutputs> = Shared::default();
        let mut fake = Fake::default();
        fake.errors.insert("bind failed".into());
        let mut listener = GameListener::new(fake, context.clone(), outputs.clone());
        assert!(!listener.is_running());
        assert_eq!(listener.source.prepared_game.as_deref(), Some("acc"));
        let mut errors = BTreeSet::from(["other".to_owned()]);
        outputs.write().take_error_changes().apply_to(&mut errors);
        assert_eq!(errors.len(), 2);
        listener.is_running();
        outputs.write().take_error_changes().apply_to(&mut errors);
        assert_eq!(errors.len(), 2, "added again, a set keeps one");
        listener.source.errors.clear();
        listener.is_running();
        outputs.write().take_error_changes().apply_to(&mut errors);
        assert!(errors.is_empty());
        listener.is_running();
        assert_eq!(outputs.write().take_error_changes(), ErrorChanges::default());
    }
}
