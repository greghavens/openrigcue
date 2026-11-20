//! Scripted listener for the dispatcher tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::listener::{Listener, Rate};

/// Deliberately keeps readable numeric snapshots behind explicit failed reads.
#[derive(Default)]
pub struct ExplicitReadSource {
    pub reads: Arc<Mutex<Vec<String>>>,
}

impl crate::Source for ExplicitReadSource {
    fn id(&self) -> &'static str {
        "api"
    }
    fn is_running(&mut self) -> Result<bool, crate::SourceError> {
        Ok(true)
    }
    fn poll(&mut self, telemetry: &mut openrigcue_model::Telemetry) -> Result<bool, crate::SourceError> {
        telemetry.speed = 20.0;
        telemetry.rpm = 1500.0;
        telemetry.gear = 3.0;
        telemetry.yaw = 31.0;
        telemetry.vehicle = "older car".into();
        telemetry.location = "older track".into();
        Ok(true)
    }
    fn native_channel(&self, _: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
        panic!("the explicit getter must not use a second legacy probe")
    }
    fn read_native(&self, name: &str) -> crate::NativeRead {
        self.reads.lock().unwrap().push(name.into());
        match name {
            "speed" | "yaw_rate" | "car_name" | "track_loc" => crate::NativeRead::Raised,
            "rpm" => crate::NativeRead::Value(openrigcue_model::native_value::LegacyValue::None),
            _ => crate::NativeRead::Unsupported,
        }
    }
}

/// Complete typed getters whose scripted values can change on each read.
pub struct ScriptedReadSource {
    pub id: &'static str,
    pub reads: Arc<Mutex<Vec<String>>>,
    answers: Mutex<HashMap<&'static str, std::collections::VecDeque<crate::NativeRead>>>,
}

impl ScriptedReadSource {
    pub fn new(id: &'static str, answers: impl IntoIterator<Item = (&'static str, Vec<crate::NativeRead>)>) -> Self {
        Self {
            id,
            reads: Arc::default(),
            answers: Mutex::new(answers.into_iter().map(|(name, values)| (name, values.into())).collect()),
        }
    }
}

impl crate::Source for ScriptedReadSource {
    fn id(&self) -> &'static str {
        self.id
    }
    fn is_running(&mut self) -> Result<bool, crate::SourceError> {
        Ok(true)
    }
    fn poll(&mut self, _: &mut openrigcue_model::Telemetry) -> Result<bool, crate::SourceError> {
        Ok(true)
    }
    fn read_native(&self, name: &str) -> crate::NativeRead {
        self.reads.lock().unwrap().push(name.into());
        match self.answers.lock().unwrap().get_mut(name) {
            Some(values) => values.pop_front().unwrap_or_else(|| panic!("unexpected extra {name} read")),
            None => crate::NativeRead::Value(openrigcue_model::native_value::LegacyValue::Int(0)),
        }
    }
}

/// Values a [`FakeListener`] reports; shared so a test can change them while the dispatcher owns the listener.
#[derive(Debug, Default)]
pub struct FakeState {
    pub running: bool,
    pub check_status_raised: bool,
    pub check_calls: usize,
    pub poll_failure: Option<super::FailurePoint>,
    pub channels: HashMap<String, f64>,
    pub native_channels: HashMap<String, openrigcue_model::native_value::LegacyValue>,
    pub rates: HashMap<&'static str, f64>,
    pub vehicle: Option<String>,
    pub location: Option<String>,
    pub receive_calls: usize,
}

/// A listener whose answers come from a [`FakeState`].
pub struct FakeListener {
    id: String,
    pub state: Arc<Mutex<FakeState>>,
}

fn rate_key(rate: Rate) -> &'static str {
    match rate {
        Rate::Yaw => "yaw_rate",
        Rate::Pitch => "pitch_rate",
        Rate::Roll => "roll_rate",
    }
}

impl FakeListener {
    pub fn new(id: &str) -> Self {
        FakeListener { id: id.to_owned(), state: Arc::default() }
    }

    pub fn set(&mut self, channel: &str, value: f64) {
        self.state.lock().unwrap().channels.insert(channel.to_owned(), value);
    }

    pub fn set_rate(&mut self, rate: Rate, value: f64) {
        self.state.lock().unwrap().rates.insert(rate_key(rate), value);
    }
}

impl Listener for FakeListener {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_running(&mut self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.check_calls += 1;
        !state.check_status_raised && state.running
    }
    fn check_status_raised(&self) -> bool {
        self.state.lock().unwrap().check_status_raised
    }
    fn failure_point(&self) -> Option<super::FailurePoint> {
        self.state.lock().unwrap().poll_failure
    }
    fn receive(&mut self) {
        self.state.lock().unwrap().receive_calls += 1;
    }
    fn channel(&self, name: &str) -> Option<f64> {
        self.state.lock().unwrap().channels.get(name).copied()
    }
    fn native_channel(&self, name: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
        self.state.lock().unwrap().native_channels.get(name).cloned()
    }
    fn rate(&self, rate: Rate) -> Option<f64> {
        self.state.lock().unwrap().rates.get(rate_key(rate)).copied()
    }
    fn vehicle(&self) -> Option<String> {
        self.state.lock().unwrap().vehicle.clone()
    }
    fn location(&self) -> Option<String> {
        self.state.lock().unwrap().location.clone()
    }
}
