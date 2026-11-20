//! Falcon BMS through its primary `FlightData` area (byte-packed,
//! 1920 bytes).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::block_reader::{BlockOpener, BridgeMirrors, Fields, KeptMapping, MappingSpec};
use super::listener_channels::begin_tick;
use super::python_numeric::{rad_to_deg, truncate_to_int};
use crate::{Source, SourceError};

/// BMS's primary shared-memory area.
pub const FALCON_BMS_MAPPING: MappingSpec = MappingSpec { name: "FalconSharedMemoryArea", size: 1920 };

/// Window title that must be open (an exact entry of the window list) for the listener to run.
pub const FALCON_BMS_WINDOW_TITLE: &str = "Falcon BMS";

/// `max_rpm` the listener reports for every aircraft.
const MAX_RPM: f64 = 10300.0;
/// Feet per second to metres per second.
const FEET_TO_METRES: f64 = 0.3048;

/// The titles of the open top-level windows, or `None`
/// while the window scan has not produced a list yet.
pub type WindowTitles = Box<dyn Fn() -> Option<Vec<String>> + Send>;

/// The fields of the flight-data area the listener reads.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FlightFrame {
    pitch: f64,
    roll: f64,
    yaw: f64,
    vt: f64,
    gs: f64,
    rpm: f64,
    nose_gear_pos: f64,
    left_gear_pos: f64,
    right_gear_pos: f64,
}

impl FlightFrame {
    fn decode(bytes: &[u8]) -> Self {
        let f = Fields(bytes);
        FlightFrame {
            pitch: f.f32(36),
            roll: f.f32(40),
            yaw: f.f32(44),
            vt: f.f32(56),
            gs: f.f32(60),
            rpm: f.f32(84),
            nose_gear_pos: f.f32(140),
            left_gear_pos: f.f32(144),
            right_gear_pos: f.f32(148),
        }
    }

    /// A block the sim has not written (the 2D UI): `rpm + roll + pitch + vt` is 0.
    fn is_blank(&self) -> bool {
        self.rpm + self.roll + self.pitch + self.vt == 0.0
    }

    /// Writes the channels.
    fn write(&self, t: &mut Telemetry) {
        let previous = begin_tick(t);
        t.rpm = truncate_to_int(self.rpm * 100.0).unwrap_or(previous.rpm);
        t.max_rpm = MAX_RPM;
        t.vertical_acceleration = self.gs;
        t.yaw = rad_to_deg(self.yaw);
        t.roll = rad_to_deg(self.roll);
        t.pitch = rad_to_deg(self.pitch);
        t.speed = self.vt * FEET_TO_METRES;
        t.landing_gear_nose = self.nose_gear_pos;
        t.landing_gear_left = self.left_gear_pos;
        t.landing_gear_right = self.right_gear_pos;
    }
}

/// The Falcon BMS listener (key `falconbms`).
pub struct FalconBms {
    opener: Box<dyn BlockOpener>,
    windows: WindowTitles,
    native_windows: Option<super::NativeWindowsReader>,
    /// The kept mapping object.
    handle: KeptMapping,
    frame: Option<FlightFrame>,
}

impl FalconBms {
    /// Listener reading the bridge mirror of [`FALCON_BMS_MAPPING`]; `windows` lists the open window titles.
    pub fn new(windows: WindowTitles) -> Self {
        Self::with_opener(Box::new(BridgeMirrors), windows)
    }

    /// Listener reading mappings through `opener`.
    pub fn with_opener(opener: Box<dyn BlockOpener>, windows: WindowTitles) -> Self {
        FalconBms { opener, windows, native_windows: None, handle: KeptMapping::new(FALCON_BMS_MAPPING), frame: None }
    }

    pub fn set_windows_reader(&mut self, reader: super::NativeWindowsReader) {
        self.native_windows = Some(reader);
    }

    /// The liveness test: a new mapping object on every call,
    /// replacing the old object after creation succeeds, then copying it; the sim is running when the block is written
    /// and a window titled exactly [`FALCON_BMS_WINDOW_TITLE`] is open.
    fn refresh(&mut self) -> Result<bool, SourceError> {
        let Ok(bytes) = self.handle.remake(self.opener.as_mut()) else { return Ok(false) };
        let frame = FlightFrame::decode(&bytes);
        self.frame = Some(frame);
        if frame.is_blank() {
            return Ok(false);
        }
        if let Some(reader) = &self.native_windows {
            let titles = reader().ok_or_else(|| {
                crate::udp_racing::unhandled::DispatcherEnds::new("required opened_windows list is unavailable")
            })?;
            return Ok(titles.iter().any(|title| title == FALCON_BMS_WINDOW_TITLE));
        }
        Ok((self.windows)().is_some_and(|titles| titles.iter().any(|title| title == FALCON_BMS_WINDOW_TITLE)))
    }
}

impl Source for FalconBms {
    fn id(&self) -> &'static str {
        "falconbms"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = match name {
            "max_rpm" => return Some(LegacyValue::Int(10300)),
            "rpm" => return crate::udp_racing::native_int(self.frame?.rpm * 100.0),
            "vertical_acceleration" => self.frame?.gs,
            "yaw" => rad_to_deg(self.frame?.yaw),
            "roll" => rad_to_deg(self.frame?.roll),
            "pitch" => rad_to_deg(self.frame?.pitch),
            "speed" => self.frame?.vt * FEET_TO_METRES,
            "landing_gear_nose" => self.frame?.nose_gear_pos,
            "landing_gear_left" => self.frame?.left_gear_pos,
            "landing_gear_right" => self.frame?.right_gear_pos,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        self.refresh()
    }

    /// The receive step is a no-op: the mapping was read by the liveness test.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view of the current mapping.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        if let Some(frame) = self.frame {
            frame.write(telemetry);
        } else {
            // The receive step always succeeds. The constant maximum rpm
            // is available even while there is no mapped data yet.
            telemetry.max_rpm = MAX_RPM;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::{BlockBuilder, FakeBlocks};
    use std::sync::{Arc, Mutex};

    fn flying_block() -> BlockBuilder {
        let mut b = BlockBuilder::new(FALCON_BMS_MAPPING.size);
        b.f32(36, 0.1) // pitch
            .f32(40, -0.2) // roll
            .f32(44, 1.5) // yaw
            .f32(56, 500.0) // vt ft/s
            .f32(60, 1.25) // gs
            .f32(84, 85.555) // rpm %
            .f32(140, 1.0) // NoseGearPos
            .f32(144, 0.5) // LeftGearPos
            .f32(148, 0.25); // RightGearPos
        b
    }

    fn titles(list: Option<Vec<&str>>) -> (WindowTitles, Arc<Mutex<Option<Vec<String>>>>) {
        let shared = Arc::new(Mutex::new(list.map(|l| l.into_iter().map(String::from).collect())));
        let reader = shared.clone();
        (Box::new(move || reader.lock().unwrap().clone()), shared)
    }

    #[test]
    fn get_channel_maps_the_block() {
        let blocks = FakeBlocks::default();
        blocks.set(FALCON_BMS_MAPPING, flying_block().build());
        let (windows, _) = titles(Some(vec!["Steam", "Falcon BMS"]));
        let mut l = FalconBms::with_opener(Box::new(blocks.clone()), windows);
        assert!(l.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(l.poll(&mut t).unwrap());
        assert_eq!(t.rpm, (f64::from(85.555f32) * 100.0).trunc());
        assert_eq!(t.max_rpm, 10300.0);
        assert_eq!(t.vertical_acceleration, 1.25);
        assert_eq!(t.yaw, rad_to_deg(1.5));
        assert_eq!(t.roll, rad_to_deg(f64::from(-0.2f32)));
        assert_eq!(t.pitch, rad_to_deg(f64::from(0.1f32)));
        assert_eq!(t.speed, 500.0 * 0.3048);
        assert_eq!(t.landing_gear_nose, 1.0);
        assert_eq!(t.landing_gear_left, 0.5);
        assert_eq!(t.landing_gear_right, 0.25);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.lateral_acceleration, 0.0);
        assert_eq!(l.native_channel("rpm"), Some(LegacyValue::Int(t.rpm as i64)));
        assert_eq!(l.native_channel("max_rpm"), Some(LegacyValue::Int(10300)));
        assert_eq!(l.native_channel("speed"), Some(LegacyValue::Float(500.0 * 0.3048)));
        assert_eq!(l.native_channel("landing_gear_left"), Some(LegacyValue::Float(0.5)));
        assert_eq!(l.native_channel("yaw_rate"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn receive_data_is_true_before_a_snapshot_while_mapped_getters_raise() {
        let (windows, _) = titles(None);
        let mut source = FalconBms::with_opener(Box::new(FakeBlocks::default()), windows);
        let mut telemetry = Telemetry { rpm: 5000.0, speed: 81.0, ..Telemetry::off() };
        assert!(source.poll(&mut telemetry).unwrap());
        assert_eq!((telemetry.rpm, telemetry.speed, telemetry.max_rpm), (5000.0, 81.0, 10300.0));
        assert!(source.channel_read_raised("rpm"));
        assert!(source.channel_read_raised("pitch"));
        assert_eq!(source.native_channel("rpm"), None);
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(10300)));
        assert_eq!(source.native_channel("gear"), Some(LegacyValue::Int(0)));
        use crate::derive::{Listener, SourceListener};
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.channel("speed"), None);
        assert_eq!(listener.native_channel("max_rpm"), Some(LegacyValue::Int(10300)));
    }

    #[test]
    fn nonfinite_rpm_conversion_raises_while_other_float_answers_remain_available() {
        let blocks = FakeBlocks::default();
        let mut block = flying_block();
        block.f32(84, f32::INFINITY);
        blocks.set(FALCON_BMS_MAPPING, block.build());
        let (windows, _) = titles(Some(vec!["Falcon BMS"]));
        let mut source = FalconBms::with_opener(Box::new(blocks), windows);
        assert!(source.is_running().unwrap());
        let mut telemetry = Telemetry { rpm: 5000.0, ..Telemetry::off() };
        assert!(source.poll(&mut telemetry).unwrap());
        assert_eq!(telemetry.rpm, 5000.0);
        assert_eq!(source.native_channel("rpm"), None);
        assert!(source.channel_read_raised("rpm"));
        assert_eq!(source.native_channel("vertical_acceleration"), Some(LegacyValue::Float(1.25)));
    }

    #[test]
    fn check_status_needs_written_block_and_exact_window_title() {
        let blocks = FakeBlocks::default();
        let (windows, shared) = titles(None);
        let mut l = FalconBms::with_opener(Box::new(blocks.clone()), windows);
        assert!(!l.is_running().unwrap(), "no mapping");
        blocks.set(FALCON_BMS_MAPPING, flying_block().build());
        assert!(!l.is_running().unwrap(), "window list not scanned yet");
        *shared.lock().unwrap() = Some(vec!["Falcon BMS 4.37".into()]);
        assert!(!l.is_running().unwrap(), "title must match exactly");
        *shared.lock().unwrap() = Some(vec!["Falcon BMS".into()]);
        assert!(l.is_running().unwrap());
        blocks.set(FALCON_BMS_MAPPING, vec![]);
        assert!(!l.is_running().unwrap(), "all-zero block");
        // rpm + roll + pitch + vt cancelling to 0 also counts as blank.
        let mut b = BlockBuilder::new(FALCON_BMS_MAPPING.size);
        b.f32(84, 1.0).f32(40, -1.0);
        blocks.set(FALCON_BMS_MAPPING, b.build());
        assert!(!l.is_running().unwrap());
    }

    #[test]
    fn native_window_read_is_after_copy_and_blank_gate_and_can_raise() {
        let blocks = FakeBlocks::default();
        let (windows, _) = titles(Some(vec!["Notepad"]));
        let current = Arc::new(Mutex::new(Some(vec!["Falcon BMS".to_owned()])));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut source = FalconBms::with_opener(Box::new(blocks.clone()), windows);
        let read = current.clone();
        let counted = calls.clone();
        source.set_windows_reader(Arc::new(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            read.lock().unwrap().clone()
        }));
        assert!(!source.is_running().unwrap());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        blocks.set(FALCON_BMS_MAPPING, flying_block().build());
        assert!(source.is_running().unwrap());
        *current.lock().unwrap() = None;
        let error = source.is_running().expect_err("required opened_windows is None after copy");
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error));
        assert!(source.native_channel("speed").is_some(), "new map was published before window failure");
        *current.lock().unwrap() = Some(vec!["Falcon BMS 4.37".into()]);
        assert!(!source.is_running().unwrap(), "title membership remains exact");
        *current.lock().unwrap() = Some(vec!["Falcon BMS".into()]);
        assert!(source.is_running().unwrap());
    }

    #[test]
    fn native_mmap_recreation_preserves_failure_and_replacement_order() {
        use crate::shared_memory::block_reader::fake::{MapAttempt, ScriptedMaps};
        let maps = ScriptedMaps::default();
        let (windows, _) = titles(Some(vec!["Falcon BMS"]));
        maps.push(MapAttempt::Bytes(flying_block().build()));
        let mut source = FalconBms::with_opener(Box::new(maps.clone()), windows);
        assert!(source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:1", "read:1"]);
        maps.push(MapAttempt::Bytes(flying_block().build()));
        assert!(source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:2", "close:1", "read:2"]);
        let previous = source.native_channel("rpm");
        maps.push(MapAttempt::CreateError);
        assert!(!source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:3"]);
        assert_eq!(source.native_channel("rpm"), previous);
        maps.push(MapAttempt::ReadError);
        assert!(!source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:4", "close:2", "read:4"]);
        assert_eq!(source.native_channel("rpm"), previous);
        maps.push(MapAttempt::Bytes(vec![0; FALCON_BMS_MAPPING.size]));
        assert!(!source.is_running().unwrap());
        assert_eq!(maps.trace(), ["create:5", "close:4", "read:5"]);
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(0)));
        drop(source);
        assert_eq!(maps.trace(), ["close:5"]);
    }

    /// Each call replaces the mapping object, including blank sections; one object remains held afterward.
    #[test]
    fn check_status_replaces_the_mapping_and_keeps_the_current_object() {
        let blocks = FakeBlocks::default();
        let (windows, _) = titles(Some(vec!["Falcon BMS"]));
        let mut l = FalconBms::with_opener(Box::new(blocks.clone()), windows);
        assert!(!l.is_running().unwrap());
        blocks.set(FALCON_BMS_MAPPING, flying_block().build());
        assert!(l.is_running().unwrap());
        blocks.set(FALCON_BMS_MAPPING, vec![]);
        assert!(!l.is_running().unwrap());
        assert!(!l.is_running().unwrap());
        assert_eq!(blocks.open_count(), 4);
        assert_eq!(blocks.held_count(FALCON_BMS_MAPPING.name), 1);
    }
}
