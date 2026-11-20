//! Space Monkey's generic telemetry provider map `GenericTelemetryProviderFiltered` (listener key
//! `spacemonkey`).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::listener_channels::begin_tick;
use super::python_numeric::rad_to_deg;
use crate::udp_flight::{AngularRates, FlightSource};
use crate::{Source, SourceError};

/// The provider data: 65 consecutive 32-bit floats.
const MAP: MappingSpec = MappingSpec { name: "GenericTelemetryProviderFiltered", size: 260 };

/// Polls with an unchanged checksum tolerated before the source counts as stopped.
const MAX_UNCHANGED_POLLS: u32 = 30;

/// Window-title fragments that name the game Space Monkey is bridging, and the vehicle id reported for each,
/// in the order they are tested.
const BRIDGED_GAMES: [(&str, &str); 9] = [
    ("Wreckfest", "wreckfest"),
    ("WRC7", "wrc7"),
    ("WRC8", "wrc8"),
    ("WRC9", "wrc9"),
    ("DiRT 5", "dirt5"),
    ("Warplanes: WW1 Fighters", "ww1fighters"),
    ("NASCAR Heat 4", "nascarheat4"),
    ("NASCAR Heat 5", "nascarheat5"),
    ("VTOLVR", "vtolvr"),
];

/// Float slot indices in the provider data.
mod slot {
    pub const YAW: usize = 2;
    pub const PITCH: usize = 3;
    pub const ROLL: usize = 4;
    pub const LOCAL_VELOCITY_X: usize = 14;
    pub const LOCAL_VELOCITY_Y: usize = 15;
    pub const LOCAL_VELOCITY_Z: usize = 16;
    pub const GFORCE_LATERAL: usize = 17;
    pub const GFORCE_LONGITUDINAL: usize = 18;
    pub const GFORCE_VERTICAL: usize = 19;
    pub const SPEED: usize = 20;
    /// Order bl, br, fl, fr.
    pub const SUSPENSION_POSITION: usize = 21;
    pub const SUSPENSION_VELOCITY: usize = 25;
    pub const GEAR: usize = 41;
    pub const ENGINE_RATE: usize = 43;
    pub const MAX_RPM: usize = 63;
}

#[derive(Debug, Clone, PartialEq)]
struct ProviderFrame {
    values: Vec<f64>,
}

impl ProviderFrame {
    fn parse(bytes: &[u8]) -> Self {
        ProviderFrame { values: (0..MAP.size / 4).map(|i| Fields(bytes).f32(4 * i)).collect() }
    }

    fn at(&self, index: usize) -> f64 {
        self.values[index]
    }

    /// The liveness checksum, in a fixed addition order.
    fn checksum(&self) -> f64 {
        let p = slot::SUSPENSION_POSITION;
        self.at(slot::ENGINE_RATE)
            + self.at(slot::SPEED)
            + self.at(slot::YAW)
            + self.at(slot::ROLL)
            + self.at(slot::PITCH)
            + self.at(p + 2)
            + self.at(p + 3)
            + self.at(p)
            + self.at(p + 1)
    }
}

/// The Space Monkey listener.
pub struct SpaceMonkey<O: BlockOpener = BridgeMirrors> {
    opener: O,
    /// The mapping object: re-made by every liveness test and kept until the next.
    handle: KeptMapping,
    latest: Option<ProviderFrame>,
    previous_checksum: f64,
    unchanged_polls: u32,
    open_windows: Option<Vec<String>>,
    windows_reader: Option<super::NativeWindowsReader>,
}

impl SpaceMonkey<BridgeMirrors> {
    pub fn new() -> Self {
        Self::with_opener(BridgeMirrors)
    }
}

impl Default for SpaceMonkey<BridgeMirrors> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: BlockOpener> SpaceMonkey<O> {
    pub fn with_opener(opener: O) -> Self {
        SpaceMonkey {
            opener,
            handle: KeptMapping::new(MAP),
            latest: None,
            previous_checksum: 0.0,
            unchanged_polls: 0,
            open_windows: None,
            windows_reader: None,
        }
    }

    /// The window titles of the dispatcher's last scan, used to
    /// name the bridged game. `None` until the first scan, in which case the vehicle name is left unchanged.
    pub fn set_open_windows(&mut self, titles: Option<Vec<String>>) {
        self.open_windows = titles;
    }

    pub fn set_windows_reader(&mut self, reader: super::NativeWindowsReader) {
        self.windows_reader = Some(reader);
    }

    /// The `car_name` answer: the first bridged game whose title fragment appears in an open window.
    fn bridged_game(&self) -> Option<String> {
        let windows = self.open_windows.as_ref()?;
        let found = BRIDGED_GAMES
            .iter()
            .find(|(fragment, _)| windows.iter().any(|title| title.contains(fragment)))
            .map(|(_, id)| (*id).to_owned());
        Some(found.unwrap_or_default())
    }

    fn native_bridged_game(&self) -> Option<String> {
        let Some(reader) = &self.windows_reader else { return self.bridged_game() };
        for (fragment, id) in BRIDGED_GAMES {
            let windows = reader()?;
            if windows.iter().any(|title| title.contains(fragment)) {
                return Some(id.to_owned());
            }
        }
        Some(String::new())
    }

    /// Writes the channels; unanswered channels read 0.
    fn write_channels(&self, frame: &ProviderFrame, t: &mut Telemetry) {
        let previous = begin_tick(t);
        t.vehicle = self.bridged_game().unwrap_or(previous.vehicle);
        let (pos, vel) = (slot::SUSPENSION_POSITION, slot::SUSPENSION_VELOCITY);
        t.rpm = frame.at(slot::ENGINE_RATE);
        t.max_rpm = frame.at(slot::MAX_RPM);
        t.gear = frame.at(slot::GEAR);
        t.speed = frame.at(slot::SPEED) * 10.0;
        t.suspension_travel_lf = frame.at(pos + 2) / 1000.0;
        t.suspension_travel_rf = frame.at(pos + 3) / 1000.0;
        t.suspension_travel_lr = frame.at(pos) / 1000.0;
        t.suspension_travel_rr = frame.at(pos + 1) / 1000.0;
        t.suspension_velocity_lf = frame.at(vel + 2);
        t.suspension_velocity_rf = frame.at(vel + 3);
        t.suspension_velocity_lr = frame.at(vel);
        t.suspension_velocity_rr = frame.at(vel + 1);
        t.lateral_acceleration = frame.at(slot::GFORCE_LATERAL);
        t.vertical_acceleration = frame.at(slot::GFORCE_VERTICAL);
        t.longitudinal_acceleration = frame.at(slot::GFORCE_LONGITUDINAL);
        t.yaw = rad_to_deg(frame.at(slot::YAW));
        t.roll = -rad_to_deg(frame.at(slot::ROLL));
        t.pitch = -rad_to_deg(frame.at(slot::PITCH));
        t.lateral_velocity = frame.at(slot::LOCAL_VELOCITY_X);
        t.longitudinal_velocity = frame.at(slot::LOCAL_VELOCITY_Y);
        t.vertical_velocity = frame.at(slot::LOCAL_VELOCITY_Z);
    }
}

impl<O: BlockOpener> Source for SpaceMonkey<O> {
    fn id(&self) -> &'static str {
        "spacemonkey"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        let f = self.latest.as_ref();
        let value = match name {
            "car_name" => {
                let game = self.native_bridged_game()?;
                return Some(if game.is_empty() { Int(0) } else { Str(game) });
            }
            "rpm" => f?.at(slot::ENGINE_RATE),
            "max_rpm" => f?.at(slot::MAX_RPM),
            "gear" => f?.at(slot::GEAR),
            "speed" => f?.at(slot::SPEED) * 10.0,
            "yaw" => rad_to_deg(f?.at(slot::YAW)),
            "roll" => -rad_to_deg(f?.at(slot::ROLL)),
            "pitch" => -rad_to_deg(f?.at(slot::PITCH)),
            "lateral_acceleration" => f?.at(slot::GFORCE_LATERAL),
            "longitudinal_acceleration" => f?.at(slot::GFORCE_LONGITUDINAL),
            "vertical_acceleration" => f?.at(slot::GFORCE_VERTICAL),
            "lateral_velocity" => f?.at(slot::LOCAL_VELOCITY_X),
            "longitudinal_velocity" => f?.at(slot::LOCAL_VELOCITY_Y),
            "vertical_velocity" => f?.at(slot::LOCAL_VELOCITY_Z),
            "suspension_travel_lf" | "suspension_travel_rf" | "suspension_travel_lr" | "suspension_travel_rr" => {
                let i = match name {
                    "suspension_travel_lf" => 2,
                    "suspension_travel_rf" => 3,
                    "suspension_travel_lr" => 0,
                    _ => 1,
                };
                f?.at(slot::SUSPENSION_POSITION + i) / 1000.0
            }
            "suspension_velocity_lf"
            | "suspension_velocity_rf"
            | "suspension_velocity_lr"
            | "suspension_velocity_rr" => {
                let i = match name {
                    "suspension_velocity_lf" => 2,
                    "suspension_velocity_rf" => 3,
                    "suspension_velocity_lr" => 0,
                    _ => 1,
                };
                f?.at(slot::SUSPENSION_VELOCITY + i)
            }
            _ => return Some(Int(0)),
        };
        Some(Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    fn read_native(&self, name: &str) -> crate::NativeRead {
        self.native_channel(name).map_or(crate::NativeRead::Raised, crate::NativeRead::Value)
    }

    /// The liveness test: alive until more than 30 consecutive polls see the same
    /// checksum (a freshly created all-zero map therefore counts as alive for its first 30 polls).
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let bytes = match self.handle.remake(&mut self.opener) {
            Ok(bytes) => bytes,
            Err(error) => {
                // A failure is printed.
                crate::printed::line(crate::printed::os_error(&error));
                return Ok(false);
            }
        };
        let frame = ProviderFrame::parse(&bytes);
        let checksum = frame.checksum();
        self.latest = Some(frame);
        if checksum == self.previous_checksum {
            self.unchanged_polls += 1;
            return Ok(self.unchanged_polls <= MAX_UNCHANGED_POLLS);
        }
        self.unchanged_polls = 0;
        self.previous_checksum = checksum;
        Ok(true)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view of the last snapshot.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        if let Some(frame) = &self.latest {
            self.write_channels(frame, telemetry);
        }
        Ok(true)
    }
}

impl<O: BlockOpener> FlightSource for SpaceMonkey<O> {
    /// The rate names read 0.
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::FakeBlocks;

    fn bytes_with(values: &[(usize, f32)]) -> Vec<u8> {
        let mut bytes = vec![0u8; MAP.size];
        for &(index, value) in values {
            bytes[4 * index..4 * index + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn source(bytes: Option<Vec<u8>>) -> SpaceMonkey<FakeBlocks> {
        let blocks = FakeBlocks::default();
        if let Some(bytes) = bytes {
            blocks.set(MAP, bytes);
        }
        SpaceMonkey::with_opener(blocks)
    }

    #[test]
    fn native_float_channels_and_window_name_results() {
        use LegacyValue::{Float, Int, Str};
        let mut s = source(None);
        assert!(s.poll(&mut Telemetry::off()).unwrap());
        assert!(s.channel_read_raised("rpm"));
        assert!(s.channel_read_raised("car_name"));
        assert_eq!(s.native_channel("unknown"), Some(Int(0)));
        s.set_open_windows(Some(vec!["Notepad".into()]));
        assert_eq!(s.native_channel("car_name"), Some(Int(0)));
        s.set_open_windows(Some(vec!["DiRT 5".into(), "Wreckfest".into()]));
        assert_eq!(s.native_channel("car_name"), Some(Str("wreckfest".into())), "name independent of frame");
        s.opener.set(
            MAP,
            bytes_with(&[(slot::ENGINE_RATE, 6500.5), (slot::MAX_RPM, 9000.0), (slot::GEAR, 3.0), (slot::SPEED, 2.5)]),
        );
        assert!(s.is_running().unwrap());
        for (name, expected) in [
            ("rpm", Float(6500.5)),
            ("max_rpm", Float(9000.0)),
            ("gear", Float(3.0)),
            ("speed", Float(25.0)),
            ("suspension_travel_lf", Float(0.0)),
            ("track_loc", Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(expected), "{name}");
        }
        s.latest.as_mut().unwrap().values[slot::ENGINE_RATE] = f64::NAN;
        assert!(matches!(s.native_channel("rpm"), Some(Float(v)) if v.is_nan()));
        assert!(!s.channel_read_raised("rpm"));
    }

    #[test]
    fn native_window_names_read_the_current_list_for_each_candidate() {
        use std::{
            collections::VecDeque,
            sync::{Arc, Mutex},
        };
        let mut source = source(None);
        source.set_open_windows(Some(vec!["Wreckfest".into()]));
        let values = Arc::new(Mutex::new(VecDeque::from([Some(vec!["Notepad".into()]), Some(vec!["WRC7".into()])])));
        let read = values.clone();
        source.set_windows_reader(Arc::new(move || read.lock().unwrap().pop_front().expect("extra window read")));
        assert_eq!(source.native_channel("car_name"), Some(LegacyValue::from("wrc7")));
        assert!(values.lock().unwrap().is_empty());
    }

    #[test]
    fn native_window_names_keep_priority_and_stop_at_required_list_failure() {
        use crate::{
            derive::{Listener, SourceListener},
            NativeRead,
        };
        use std::{
            collections::VecDeque,
            sync::{Arc, Mutex},
        };
        let mut source = source(None);
        let values = Arc::new(Mutex::new(VecDeque::new()));
        let read = values.clone();
        source.set_windows_reader(Arc::new(move || read.lock().unwrap().pop_front().expect("extra read")));
        let listener = SourceListener::new(source);
        for (index, (fragment, id)) in BRIDGED_GAMES.iter().enumerate() {
            let mut queued = values.lock().unwrap();
            queued.extend((0..index).map(|_| Some(Vec::new())));
            queued.push_back(Some(vec![format!("{fragment} window")]));
            drop(queued);
            assert_eq!(listener.read_native("car_name"), NativeRead::Value(LegacyValue::from(*id)));
            assert!(values.lock().unwrap().is_empty());
        }
        values.lock().unwrap().extend([Some(Vec::new()), None]);
        assert_eq!(listener.read_native("car_name"), NativeRead::Raised);
        assert!(values.lock().unwrap().is_empty());
        values.lock().unwrap().extend((0..BRIDGED_GAMES.len()).map(|_| Some(Vec::new())));
        assert_eq!(listener.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(listener.read_native("unknown"), NativeRead::Value(LegacyValue::Int(0)));
        assert!(values.lock().unwrap().is_empty(), "unrelated getter reads no window list");
    }

    /// Liveness: the all-zero map matches the initial checksum 0, so it is alive for 30
    /// polls and stopped from the 31st on; a change resets the count.
    #[test]
    fn checksum_staleness_counter() {
        let mut s = source(None);
        for _ in 0..30 {
            assert!(s.is_running().unwrap());
        }
        assert!(!s.is_running().unwrap());
        assert!(!s.is_running().unwrap());
        s.opener.set(MAP, bytes_with(&[(slot::SPEED, 3.0)]));
        assert!(s.is_running().unwrap());
        assert_eq!(s.unchanged_polls, 0);
        for _ in 0..30 {
            assert!(s.is_running().unwrap());
        }
        assert!(!s.is_running().unwrap());
    }

    /// Channel mapping, including speed * 10, /1000 suspension and negated roll/pitch.
    #[test]
    fn get_channel_mapping() {
        let mut s = source(Some(bytes_with(&[
            (slot::ENGINE_RATE, 4000.0),
            (slot::MAX_RPM, 8000.0),
            (slot::GEAR, 2.0),
            (slot::SPEED, 3.5),
            (slot::SUSPENSION_POSITION, 10.0),
            (slot::SUSPENSION_POSITION + 1, 20.0),
            (slot::SUSPENSION_POSITION + 2, 30.0),
            (slot::SUSPENSION_POSITION + 3, 40.0),
            (slot::SUSPENSION_VELOCITY + 2, 0.5),
            (slot::GFORCE_LATERAL, 0.25),
            (slot::YAW, 1.0),
            (slot::ROLL, 0.5),
            (slot::PITCH, -0.5),
            (slot::LOCAL_VELOCITY_Y, 12.0),
        ])));
        s.set_open_windows(Some(vec!["Steam".into(), "DiRT 5 - DX12".into()]));
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (4000.0, 8000.0, 2.0, 35.0));
        assert_eq!(
            (t.suspension_travel_lf, t.suspension_travel_rf, t.suspension_travel_lr, t.suspension_travel_rr),
            (0.03, 0.04, 0.01, 0.02)
        );
        assert_eq!(t.suspension_velocity_lf, 0.5);
        assert_eq!(t.lateral_acceleration, 0.25);
        assert_eq!(t.yaw, rad_to_deg(1.0));
        assert_eq!(t.roll, -rad_to_deg(0.5));
        assert_eq!(t.pitch, -rad_to_deg(-0.5));
        assert_eq!(t.longitudinal_velocity, 12.0);
        assert_eq!(t.vehicle, "dirt5");
    }

    /// car_name: first match in table order, 0 (empty) when nothing matches.
    #[test]
    fn bridged_game_order() {
        let mut s = source(None);
        assert_eq!(s.bridged_game(), None);
        s.set_open_windows(Some(vec!["NASCAR Heat 5".into(), "Wreckfest".into()]));
        assert_eq!(s.bridged_game().as_deref(), Some("wreckfest"));
        s.set_open_windows(Some(vec!["Notepad".into()]));
        assert_eq!(s.bridged_game().as_deref(), Some(""));
    }
}
