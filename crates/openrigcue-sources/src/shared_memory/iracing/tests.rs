//! Behaviour tests of the iRacing listener.

use super::memory::test_map::{MapBuilder, FLOAT};
use super::*;
use crate::shared_memory::block_reader::fake::FakeBlocks;
use crate::udp_racing::unhandled::ends_dispatcher;
use std::sync::Arc;

/// A simulator status probe with a fixed answer.
struct FakeSim(bool);

impl SimStatus for FakeSim {
    fn is_running(&mut self) -> Result<bool, Raised> {
        Ok(self.0)
    }
}

/// A data-valid event with scripted outcomes; counts the waits.
#[derive(Default)]
struct FakeEvent {
    /// `None`: OpenEventW fails; `Some(signalled)`: the event opens and the 32 ms wait returns `signalled`.
    startup: Option<bool>,
    waits: usize,
    forgets: usize,
    timeout: bool,
    after_wait: Option<Box<dyn FnMut() + Send>>,
}

impl FakeEvent {
    fn signalling() -> Self {
        FakeEvent { startup: Some(true), ..Self::default() }
    }
}

impl DataValidEvent for FakeEvent {
    fn open_and_wait(&mut self) -> bool {
        self.startup == Some(true)
    }

    fn wait(&mut self) -> bool {
        self.waits += 1;
        if let Some(after) = &mut self.after_wait {
            after();
        }
        !self.timeout
    }
    fn forget(&mut self) {
        self.forgets += 1;
    }
}

type TestIracing = Iracing<FakeBlocks, FakeSim, FakeEvent>;

fn simulator(sim: bool, event: FakeEvent) -> Simulator<FakeSim, FakeEvent> {
    Simulator { status: FakeSim(sim), data_valid: event }
}

const SESSION: &str = "---\nWeekendInfo:\n TrackDisplayShortName: Okayama\n\nDriverInfo:\n DriverCarIdx: 3\n DriverCarSLBlinkRPM: 7400.000\n Drivers:\n - CarIdx: 0\n   CarScreenName: Pace Car\n - CarIdx: 3\n   UserName: A. Driver\n   CarScreenName: Mazda MX-5 Cup\n\n";

/// A connected session in the car, driving.
fn driving() -> MapBuilder {
    let mut map = MapBuilder::new(3);
    map.boolean("IsReplayPlaying", false)
        .boolean("IsOnTrackCar", true)
        .int("SessionNum", 0)
        .float("Speed", 30.0)
        .float("RPM", 5500.0)
        .int("Gear", 4)
        .float("SteeringWheelAngle", 0.5)
        .float("LatAccel", 3.0)
        .float("LongAccel", -2.0)
        .float("VertAccel", 9.8)
        .float("VelocityX", 29.5)
        .float("VelocityY", 1.5)
        .float("VelocityZ", 0.25)
        .float("Yaw", 1.0)
        .float("Pitch", 0.1)
        .float("Roll", -0.2)
        .float("YawRate", 0.3)
        .float("LFshockDefl", 0.05)
        .float("RRshockDefl", 0.07)
        .float("LFshockVel", 0.1)
        .float("TireLF_RumblePitch", 33.0)
        .float("TireRF_RumblePitch", 0.0)
        .float("TireLR_RumblePitch", 0.0)
        .float("TireRR_RumblePitch", 0.0)
        .bits("SessionFlags", 0x0000_0004)
        .int("CarLeftRight", 4)
        .boolean("BrakeABSactive", true)
        .float("OilPress", 4.5)
        .float("ManifoldPress", 1.25)
        .float("OilTemp", 98.0)
        .int("Lap", 7)
        .float("LapDistPct", 0.5)
        .float("FuelLevelPct", 0.75)
        .float("Throttle", 1.0)
        .float("Brake", 0.0)
        .float("Clutch", 0.0)
        .float("LFtempCM", 80.0)
        .float("LFtempCR", 78.0)
        .float("LFtempCL", 82.0)
        .float("RRwearM", 0.9)
        .float("RRwearR", 0.95)
        .float("RRwearL", 0.85)
        .session(SESSION);
    map
}

fn listener(map: &MapBuilder, sim: bool) -> TestIracing {
    let pages = FakeBlocks::default();
    pages.set(MEMORY, map.build());
    Iracing::with_parts(pages, simulator(sim, FakeEvent::signalling()), IracingOptions::default())
}

fn connected(map: &MapBuilder) -> TestIracing {
    let mut iracing = listener(map, true);
    assert!(iracing.is_running().unwrap());
    iracing
}

/// One fresh shared-memory image per lookup; no game process or actual mapping is involved.
struct ChangingMemory {
    frames: std::collections::VecDeque<Option<Vec<u8>>>,
    current: Vec<u8>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl BlockReader for ChangingMemory {
    fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
        if matches!(offset, 48 | 12) {
            self.read()?;
        } else if self.current.is_empty() {
            self.current = self.frames.pop_front().expect("missing initial header").expect("initial header read");
        }
        Ok(self.current.get(offset..offset + len).ok_or(std::io::ErrorKind::UnexpectedEof)?.to_vec())
    }

    fn read(&mut self) -> std::io::Result<&[u8]> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.frames.pop_front().expect("unexpected extra shared-memory lookup") {
            Some(bytes) => {
                self.current = bytes;
                Ok(&self.current)
            }
            None => Err(std::io::Error::other("scripted map read failure")),
        }
    }
}

fn changing_memory(source: &mut TestIracing, frames: Vec<Option<Vec<u8>>>) -> Arc<std::sync::atomic::AtomicUsize> {
    source.live_value("IsReplayPlaying").unwrap(); // initialize the independent name cache
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    *source.shared_mem.get_mut().unwrap() =
        Some(Box::new(ChangingMemory { frames: frames.into(), current: Vec::new(), reads: reads.clone() }));
    reads
}

struct ScriptedFields(std::collections::VecDeque<(usize, i32)>);
impl BlockReader for ScriptedFields {
    fn read(&mut self) -> std::io::Result<&[u8]> {
        panic!("SDK lifecycle fields must not copy the full mapping")
    }
    fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
        let (expected, value) = self.0.pop_front().expect("unexpected SDK field read");
        assert_eq!((offset, len), (expected, 4));
        Ok(value.to_le_bytes().to_vec())
    }
}

struct ScriptedRanges(std::collections::VecDeque<(usize, Vec<u8>)>);
impl BlockReader for ScriptedRanges {
    fn read(&mut self) -> std::io::Result<&[u8]> {
        panic!("unexpected whole-map copy")
    }
    fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
        let (expected, bytes) = self.0.pop_front().expect("unexpected range read");
        assert_eq!((offset, len), (expected, bytes.len()));
        Ok(bytes)
    }
}
fn word(offset: usize, value: i32) -> (usize, Vec<u8>) {
    (offset, value.to_le_bytes().to_vec())
}
fn name(offset: usize, value: &str) -> (usize, Vec<u8>) {
    let mut bytes = vec![0; 32];
    bytes[..value.len()].copy_from_slice(value.as_bytes());
    (offset, bytes)
}

#[test]
fn lazy_headers_build_positions_before_names_and_read_return_count_again() {
    let mut sdk = Connection { buffers: Some(Buffers::from_lengths(vec![16, 16])), ..Default::default() };
    let mut input = ScriptedRanges(
        [
            word(24, 2),
            word(28, 100),
            word(28, 200), // second object is at200+144
            name(116, "Speed"),
            name(360, "RPM"),
            word(48, 20),
            word(64, 30), // second-newest is index0
            word(100, 4),
            word(108, 1), // decode a float, count 1
            word(52, 500),
            word(104, 8), // current buffer offset before var offset
            (508, 42.0_f32.to_le_bytes().to_vec()),
            word(108, 2), // return classification now sees count2 -> list
        ]
        .into(),
    );
    assert_eq!(sdk.get_live(&mut input, "Speed").unwrap(), Value::Var(VarValue::Many(vec![Scalar::Float(42.0)])));
    assert!(input.0.is_empty());
    assert_eq!(sdk.variables.as_ref().unwrap().header_position("RPM"), Some(344));
}

#[test]
fn lazy_dictionary_name_failure_keeps_earlier_names_without_retrying_later_names() {
    let mut sdk = Connection { buffers: Some(Buffers::from_lengths(vec![16, 16])), ..Default::default() };
    struct FailName(ScriptedRanges);
    impl BlockReader for FailName {
        fn read(&mut self) -> std::io::Result<&[u8]> {
            unreachable!()
        }
        fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
            if offset == 260 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            self.0.read_range(offset, len)
        }
    }
    let mut failed = FailName(ScriptedRanges([word(24, 2), word(28, 100), word(28, 100), name(116, "Speed")].into()));
    assert!(sdk.get_live(&mut failed, "Speed").is_err());
    assert_eq!(sdk.variable_positions.as_ref().unwrap(), &[100, 244]);
    let mut next = ScriptedRanges(
        [
            word(48, 20),
            word(64, 30),
            word(100, 4),
            word(108, 1),
            word(52, 500),
            word(104, 0),
            (500, 3_f32.to_le_bytes().to_vec()),
            word(108, 1),
        ]
        .into(),
    );
    assert_eq!(sdk.get_live(&mut next, "Speed").unwrap(), Value::Var(VarValue::One(Scalar::Float(3.0))));
    assert!(next.0.is_empty());
}

#[test]
fn invalid_variable_type_raises_before_reading_count_or_values() {
    let mut sdk = Connection { buffers: Some(Buffers::from_lengths(vec![16, 16])), ..Default::default() };
    let mut input = ScriptedRanges(
        [
            word(24, 1),
            word(28, 100),
            name(116, "Speed"),
            word(48, 20),
            word(64, 30),
            word(100, 99),
            word(108, 1), // must remain unread after type code 99 fails
        ]
        .into(),
    );
    assert!(sdk.get_live(&mut input, "Speed").is_err());
    assert_eq!(input.0, [word(108, 1)]);
}

#[test]
fn native_freeze_reads_tick_order_then_slice_start_and_stop_separately() {
    let mut source = connected(&driving());
    source.sdk.get_mut().unwrap().buffers = Some(Buffers::from_lengths(vec![4, 4]));
    *source.shared_mem.get_mut().unwrap() = Some(Box::new(ScriptedRanges(
        [
            word(48, 30),
            word(64, 20), // newest object0
            word(52, 500),
            word(52, 502), // slice500:502+4
            (500, b"ABCDEF".to_vec()),
        ]
        .into(),
    )));
    assert!(source.receive_native(&mut Telemetry::off()).unwrap());
    assert_eq!(source.sdk.get_mut().unwrap().frozen, Some((0, b"ABCDEF".to_vec())));
    assert_eq!(source.simulator.data_valid.waits, 1);
}

#[test]
fn session_cache_invalidates_before_a_later_region_read_failure() {
    let mut source = connected(&driving());
    assert!(source
        .sdk
        .get_mut()
        .unwrap()
        .get_live(source.shared_mem.get_mut().unwrap().as_mut().unwrap().as_mut(), "DriverInfo")
        .is_ok());
    assert!(source.sdk.get_mut().unwrap().session.cached("DriverInfo").is_some());
    let mut input = ScriptedRanges([word(12, 2), word(12, 3)].into());
    struct FailOffset(ScriptedRanges);
    impl BlockReader for FailOffset {
        fn read(&mut self) -> std::io::Result<&[u8]> {
            unreachable!()
        }
        fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
            if offset == 20 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            self.0.read_range(offset, len)
        }
    }
    let sdk = source.sdk.get_mut().unwrap();
    assert!(sdk.get_live(&mut FailOffset(input), "DriverInfo").is_err());
    assert!(sdk.session.cached("DriverInfo").is_none());
    assert!(!sdk.session.needs_update(3), "the second counter read is the assigned value");
    input = ScriptedRanges([word(12, 3), word(20, 0), word(16, 0), (0, vec![])].into());
    assert_eq!(
        sdk.get_live(&mut input, "DriverInfo").unwrap(),
        Value::None,
        "failed refresh must not expose stale cached data"
    );
}

#[test]
fn native_lap_aliases_read_the_original_sdk_variable_names() {
    let mut map = driving();
    for (name, value) in [
        ("LapBestLap", 2.0),
        ("LapBestLapTime", 71.5),
        ("LapLastLapTime", 72.0),
        ("LapCurrentLapTime", 19.0),
        ("LapDeltaToBestLap", -0.3),
        ("LapDeltaToOptimalLap", -0.2),
        ("LapDeltaToSessionBestLap", 0.4),
        ("LapDeltaToSessionOptimalLap", 0.7),
        ("LapDeltaToSessionLastLap", 0.8),
    ] {
        map.float(name, value);
    }
    let source = connected(&map);
    for (name, expected) in [
        ("lap_best_lap", 2.0_f32),
        ("lap_best_lap_time", 71.5),
        ("lap_last_lap_time", 72.0),
        ("lap_current_lap_time", 19.0),
        ("lap_delta_to_best_lap", -0.3),
        ("lap_delta_to_optimal_lap", -0.2),
        ("lap_delta_to_session_best_lap", 0.4),
        ("lap_delta_to_session_optimal_lap", 0.7),
        ("lap_delta_to_session_last_lap", 0.8),
    ] {
        assert_eq!(source.read_native(name), NativeRead::Value(LegacyValue::Float(f64::from(expected))));
    }
}

#[test]
fn startup_reads_buffer_lengths_in_constructor_order_then_current_version() {
    let mut fields = ScriptedFields([(32, 2), (36, 2), (36, 5), (0, 1)].into());
    let mut sdk = Connection::default();
    assert!(sdk.attach_live(&mut fields).unwrap());
    assert!(fields.0.is_empty());
    let mut memory = driving().build();
    // Buffer1 is newest; its constructor captured length5, not buffer0 length2.
    memory[48..52].copy_from_slice(&200_i32.to_le_bytes());
    memory[64..68].copy_from_slice(&201_i32.to_le_bytes());
    let offset = i32::from_le_bytes(memory[68..72].try_into().unwrap()) as usize;
    memory[offset..offset + 5].copy_from_slice(b"ABCDE");
    assert_eq!(sdk.buffers.as_ref().unwrap().freeze_newest(&memory), Some((1, b"ABCDE".to_vec())));
}

#[test]
fn startup_header_error_retains_old_header_until_replacement_succeeds() {
    let mut sdk = Connection { buffers: Some(Buffers::from_lengths(vec![9])), ..Default::default() };
    let mut fields = ScriptedFields([(32, 1), (36, 4)].into());
    // Failure in the version read happens after the new header has been assigned.
    struct FailVersion(ScriptedFields);
    impl BlockReader for FailVersion {
        fn read(&mut self) -> std::io::Result<&[u8]> {
            unreachable!()
        }
        fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
            if offset == 0 {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            self.0.read_range(offset, len)
        }
    }
    assert!(sdk.attach_live(&mut FailVersion(fields)).is_err());
    assert_eq!(sdk.buffers.as_ref().unwrap().descriptor_bytes(), Some(64));
    fields = ScriptedFields([(32, 0), (0, 1)].into());
    assert!(!sdk.attach_live(&mut fields).unwrap());
    assert!(sdk.buffers.as_ref().unwrap().is_empty());
}

#[test]
fn connected_status_reloads_before_final_return() {
    let mut sdk = Connection { buffers: Some(Buffers::from_lengths(vec![4, 4])), ..Default::default() };
    // First two status reads are connected; the final current read is disconnected.
    let mut fields = ScriptedFields([(4, 1), (4, 1), (4, 0)].into());
    assert!(!sdk.is_connected_live(&mut fields).unwrap());
    assert!(fields.0.is_empty());
    assert_eq!(sdk.workaround_state, 0);
}

#[test]
fn connected_check_repeats_values_and_keeps_speed_assignment_when_rpm_read_raises() {
    let mut source = connected(&driving());
    let mut newer = driving();
    newer.raw("Speed", 0, &77.0_f32.to_le_bytes());
    source.previous.rpm = Some(1234.0);
    let reads = changing_memory(
        &mut source,
        vec![
            Some(driving().build()), // connected test
            Some(driving().build()), // Speed is present
            Some(newer.build()),     // the previous-speed assignment reads again
            None,                    // RPM predicate fails after the speed write
        ],
    );
    assert!(ends_dispatcher(&source.is_running().unwrap_err()));
    assert_eq!(source.previous.speed, Some(77.0));
    assert_eq!(source.previous.rpm, Some(1234.0));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 3, "status reads do not copy a full frame");
}

#[test]
fn native_getter_reloads_the_map_for_predicate_and_return_reads() {
    let mut source = connected(&driving());
    let mut newer = driving();
    newer.raw("Speed", 0, &77.0_f32.to_le_bytes());
    let reads = changing_memory(
        &mut source,
        vec![
            Some(driving().build()), // replay predicate
            Some(driving().build()), // replay truthiness
            Some(driving().build()), // on track
            Some(driving().build()), // speed is present
            Some(newer.build()),     // returned speed reads again
        ],
    );
    assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Float(77.0)));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 5);
}

#[test]
fn native_engine_gate_reloads_rpm_and_can_skip_the_later_return() {
    let mut source = connected(&driving());
    let mut slow = driving();
    slow.raw("RPM", 0, &100.0_f32.to_le_bytes()).raw("Speed", 0, &0.5_f32.to_le_bytes());
    let reads = changing_memory(
        &mut source,
        vec![
            Some(driving().build()),
            Some(driving().build()),
            Some(driving().build()),
            Some(driving().build()), // Speed is present
            Some(driving().build()), // RPM is present
            Some(slow.build()),      // current RPM < 400
            Some(slow.build()),      // current Speed < 1
        ],
    );
    assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 7);
}

#[test]
fn native_flag_reloads_each_priority_mask_and_short_circuits_on_the_match() {
    let mut source = connected(&driving());
    let mut blue = driving();
    blue.raw("SessionFlags", 0, &32_i32.to_le_bytes());
    let mut frames = vec![Some(driving().build()); 6]; // pause and engine predicates
    frames.push(Some(driving().build())); // green-held check: value4 does not match1024
    frames.push(Some(blue.build())); // blue check reads the new flag value
    let reads = changing_memory(&mut source, frames);
    assert_eq!(source.read_native("flag"), NativeRead::Value(LegacyValue::Int(2)));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 8);
}

#[test]
fn native_session_maximum_uses_second_driver_info_read() {
    let mut source = connected(&driving());
    let mut changed = driving();
    changed.session(&SESSION.replace("7400.000", "8123.000"));
    let mut frames = vec![Some(driving().build()); 5]; // three pause lookups, then two update-counter reads
    frames.extend([Some(changed.build()), Some(changed.build())]); // second DriverInfo lookup and assignment
    let reads = changing_memory(&mut source, frames);
    assert_eq!(source.read_native("max_rpm"), NativeRead::Value(LegacyValue::Float(8123.0)));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 7);
}

#[test]
fn native_acceleration_caught_read_failure_converts_fallback_to_float() {
    let mut source = connected(&driving());
    let mut frames = vec![Some(driving().build()); 6]; // pause and engine predicates
    frames.push(None); // the failed acceleration lookup is handled and reads 0
    changing_memory(&mut source, frames);
    assert_eq!(source.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Float(0.0)));
}

#[test]
fn native_full_frame_reads_stay_bounded_with_a_live_memory_reader() {
    let source = connected(&driving());
    let mut channels: Vec<&str> = Telemetry::CHANNELS.to_vec();
    channels.extend(["car_name", "track_loc", "yaw_rate", "pitch_rate", "roll_rate"]);
    let started = std::time::Instant::now();
    for _ in 0..100 {
        for channel in &channels {
            std::hint::black_box(source.read_native(channel));
        }
    }
    eprintln!("100 native iRacing getter frames ({} channels/frame): {:?}", channels.len(), started.elapsed());
}

#[test]
fn native_range_getters_do_not_fall_back_to_full_map_copies_after_header_setup() {
    struct Counted {
        bytes: Vec<u8>,
        full: Arc<std::sync::atomic::AtomicUsize>,
        ranged: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl BlockReader for Counted {
        fn read(&mut self) -> std::io::Result<&[u8]> {
            self.full.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(&self.bytes)
        }
        fn read_range(&mut self, offset: usize, len: usize) -> std::io::Result<Vec<u8>> {
            self.ranged.fetch_add(len, std::sync::atomic::Ordering::SeqCst);
            Ok(self.bytes[offset..offset + len].to_vec())
        }
    }
    let mut source = connected(&driving());
    let full = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ranged = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    *source.shared_mem.get_mut().unwrap() =
        Some(Box::new(Counted { bytes: driving().build(), full: full.clone(), ranged: ranged.clone() }));
    assert!(matches!(source.read_native("rpm"), NativeRead::Value(_))); // builds the cached name table
    full.store(0, std::sync::atomic::Ordering::SeqCst);
    ranged.store(0, std::sync::atomic::Ordering::SeqCst);
    for name in ["speed", "rpm", "flag", "max_rpm", "track_loc"] {
        assert!(matches!(source.read_native(name), NativeRead::Value(_)));
    }
    assert_eq!(full.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(ranged.load(std::sync::atomic::Ordering::SeqCst) < MEMORY.size / 100);
}

#[test]
fn native_iracing_pause_is_required_even_when_the_car_is_on_track() {
    let mut source = connected(&driving());
    source.set_pause_reader(std::sync::Arc::new(|| None));
    assert_eq!(source.read_native("speed"), NativeRead::Raised);
    assert_eq!(source.read_native("unknown"), NativeRead::Raised);
}

#[test]
fn native_iracing_pause_reads_again_after_replay_does_not_block() {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    let mut map = driving();
    map.raw("IsReplayPlaying", 0, &[1]).raw("IsOnTrackCar", 0, &[0]);
    let mut source = connected(&map);
    let values = Arc::new(Mutex::new(VecDeque::from([Some(LegacyValue::from("no")), None])));
    source.set_pause_reader({
        let values = values.clone();
        Arc::new(move || values.lock().unwrap().pop_front().expect("one value per source read"))
    });
    assert_eq!(source.read_native("speed"), NativeRead::Raised, "second required lookup follows replay branch");
    assert!(values.lock().unwrap().is_empty());
}

#[test]
fn native_iracing_pause_short_circuit_recovery_and_native_receive_order() {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    let mut map = driving();
    map.raw("IsReplayPlaying", 0, &[1]).raw("IsOnTrackCar", 0, &[0]);
    let mut source = connected(&map);
    let values = Arc::new(Mutex::new(VecDeque::from([Some(LegacyValue::from("yes"))])));
    source.set_pause_reader({
        let values = values.clone();
        Arc::new(move || values.lock().unwrap().pop_front().expect("pause read count"))
    });
    assert!(source.receive_native(&mut Telemetry::off()).unwrap());
    assert_eq!(values.lock().unwrap().len(), 1, "receiveData does not read pause");
    assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Int(0)));
    assert!(values.lock().unwrap().is_empty(), "replay true skips the second branch");
    for answers in [
        [Some(LegacyValue::from("no")), Some(LegacyValue::from("no"))],
        [Some(LegacyValue::Bool(false)), Some(LegacyValue::Bool(false))],
    ] {
        values.lock().unwrap().extend(answers);
        assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Float(30.0)));
        assert!(values.lock().unwrap().is_empty());
    }
    values.lock().unwrap().push_back(Some(LegacyValue::from("bad")));
    assert_eq!(source.read_native("unknown"), NativeRead::Raised);
    assert!(values.lock().unwrap().is_empty());
}

#[test]
fn native_iracing_pause_is_not_read_before_missing_replay_fails() {
    let mut map = MapBuilder::new(3);
    map.var("IsReplayPlaying", 99, 1);
    let mut source = connected(&map);
    source.set_pause_reader(std::sync::Arc::new(|| panic!("pause must follow replay lookup")));
    // An invalid variable type makes the first header read fail.
    assert_eq!(source.read_native("speed"), NativeRead::Raised);
}

#[test]
fn native_iracing_dams_reads_current_required_mode_in_steering_branch() {
    let mut source = connected(&driving());
    source.set_dams_reader(std::sync::Arc::new(|| Some(LegacyValue::from("2"))));
    assert_eq!(source.read_native("steering_wheel_angle"), NativeRead::Value(LegacyValue::Float(0.5)));
}

#[test]
fn native_iracing_dams_missing_mode_raises_instead_of_using_cached_default() {
    let mut source = connected(&driving());
    source.set_dams_reader(std::sync::Arc::new(|| None));
    assert_eq!(source.read_native("steering_wheel_angle"), NativeRead::Raised);
}

#[test]
fn native_iracing_dams_keeps_int_conversion_and_late_gate_order() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    let mut source = connected(&driving());
    let value = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    source.set_dams_reader({
        let (value, calls) = (value.clone(), calls.clone());
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            value.lock().unwrap().clone()
        })
    });
    assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Float(30.0)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for (input, steering) in [
        (LegacyValue::from(" +００2 "), true),
        (LegacyValue::Float(2.9), true),
        (LegacyValue::Bytes(b" 2 ".to_vec()), true),
        (LegacyValue::Bool(true), false),
        (LegacyValue::Int(-7), false),
        (LegacyValue::from("9999999999999999999999999999999999999999999999"), false),
        (LegacyValue::Float(1e200), false),
    ] {
        *value.lock().unwrap() = Some(input);
        assert_eq!(
            source.read_native("steering_wheel_angle"),
            NativeRead::Value(LegacyValue::Float(if steering { 0.5 } else { 3.0 }))
        );
    }
    for input in
        [LegacyValue::None, LegacyValue::from("2.0"), LegacyValue::Float(f64::NAN), LegacyValue::Float(f64::INFINITY)]
    {
        *value.lock().unwrap() = Some(input);
        assert_eq!(source.read_native("steering_wheel_angle"), NativeRead::Raised);
    }
    source.set_pause_reader(Arc::new(|| Some(LegacyValue::from("yes"))));
    let mut replay = driving();
    replay.raw("IsReplayPlaying", 0, &[1]);
    source.opener.set(MEMORY, replay.build());
    let before = calls.load(Ordering::SeqCst);
    assert_eq!(source.read_native("steering_wheel_angle"), NativeRead::Value(LegacyValue::Int(0)));
    assert_eq!(calls.load(Ordering::SeqCst), before, "pause precedes mode conversion");
}

fn poll(iracing: &mut TestIracing) -> Telemetry {
    let mut t = Telemetry::off();
    assert!(iracing.poll(&mut t).unwrap());
    t
}

#[test]
fn native_receive_does_not_predict_none_failure() {
    use crate::derive::{Listener, SourceListener};
    let mut map = MapBuilder::new(3);
    map.boolean("IsReplayPlaying", false).boolean("IsOnTrackCar", true).float("RPM", 5500.0);
    let mut iracing = connected(&map);
    assert!(iracing.receive_native(&mut Telemetry::off()).is_ok());
    assert_eq!(iracing.native_channel("speed"), Some(LegacyValue::None));
    assert_eq!(iracing.native_channel("rpm"), Some(LegacyValue::Float(5500.0)));
    let mut listener = SourceListener::new(iracing);
    listener.receive();
    assert_eq!(listener.failure_point(), None);
    assert_eq!(listener.read_native("speed"), NativeRead::Value(LegacyValue::None));
}

#[test]
fn native_sdk_kinds_and_actual_adapter_reads() {
    use crate::derive::{Listener, SourceListener};
    let mut map = driving();
    map.boolean("ExtraBool", true)
        .var("ByteTag", 0, 1)
        .raw("ByteTag", 0, b"A")
        .var("ExtraList", FLOAT, 2)
        .raw("ExtraList", 0, &2.5f32.to_le_bytes())
        .raw("ExtraList", 1, &3.5f32.to_le_bytes())
        .float("SteeringWheelAngleMax", 7.0);
    let iracing = connected(&map);
    let mut listener = SourceListener::new(iracing);
    listener.receive();
    assert_eq!(listener.failure_point(), None);
    for (name, expected) in [
        ("speed", LegacyValue::Float(30.0)),
        ("rpm", LegacyValue::Float(5500.0)),
        ("gear", LegacyValue::Int(4)),
        ("lap", LegacyValue::Int(7)),
        ("max_rpm", LegacyValue::Float(7400.0)),
        ("flag", LegacyValue::Int(1)),
        ("raw_flag", LegacyValue::Int(4)),
        ("spotter_left", LegacyValue::Int(1)),
        ("abs_lock", LegacyValue::Int(1)),
        ("fuel_level", LegacyValue::Float(0.75)),
        ("ExtraBool", LegacyValue::Bool(true)),
        ("ByteTag", LegacyValue::Bytes(vec![b'A'])),
        ("ExtraList", LegacyValue::List(vec![LegacyValue::Float(2.5), LegacyValue::Float(3.5)])),
        ("steering_wheel_angle_max", LegacyValue::Float(7.0)),
        ("car_name", LegacyValue::Str("Mazda MX-5 Cup".into())),
        ("track_loc", LegacyValue::Str("Okayama".into())),
        ("suspension_velocity_rr", LegacyValue::Int(0)),
        ("unknown", LegacyValue::Int(0)),
    ] {
        assert_eq!(listener.native_channel(name), Some(expected), "{name}");
    }
    assert_eq!(listener.read_native("tyre_temperature_rr"), NativeRead::Raised);
}

#[test]
fn native_signed_offsets_reach_the_actual_speed_getter() {
    use super::memory::test_map::{BUFFER_BASE, BUFFER_LEN, VAR_HEADERS};
    use crate::derive::{Listener, SourceListener};

    let mut map = driving();
    // Speed is the fourth header; the second-newest live buffer is buffer one.
    map.header_field(VAR_HEADERS + 3 * 144 + 4, -4);
    let start = BUFFER_BASE + BUFFER_LEN - 4;
    map.memory[start..start + 4].copy_from_slice(&29_f32.to_le_bytes());
    let mut listener = SourceListener::new(connected(&map));
    listener.receive();
    assert_eq!(listener.failure_point(), None);
    assert_eq!(listener.read_native("speed"), NativeRead::Value(LegacyValue::Float(29.0)));
}

#[test]
fn native_frozen_buffer_follows_object_identity_when_order_changes() {
    let mut iracing = connected(&driving());
    iracing.receive_native(&mut Telemetry::off()).unwrap(); // buffer 2 is newest and becomes frozen
    let mut changed = driving();
    changed.raw("RPM", 0, &7777.0f32.to_le_bytes()).header_field(48 + 16, 103);
    iracing.opener.set(MEMORY, changed.build()); // buffer 1 newest; frozen object 2 is now second
    assert_eq!(iracing.native_channel("rpm"), Some(LegacyValue::Float(5500.0)));
    changed.header_field(super::memory::test_map::VAR_HEADERS + 4 * 144, super::memory::test_map::INT);
    iracing.opener.set(MEMORY, changed.build());
    assert_eq!(
        iracing.native_channel("rpm"),
        Some(LegacyValue::Int(i64::from(5500.0f32.to_bits()))),
        "the variable header remains live even though the data is frozen"
    );
    changed.header_field(super::memory::test_map::VAR_HEADERS + 4 * 144, FLOAT);
    iracing.opener.set(MEMORY, changed.build());
    iracing.receive_native(&mut Telemetry::off()).unwrap(); // unfreeze 2, freeze 1
    assert_eq!(iracing.native_channel("rpm"), Some(LegacyValue::Float(7777.0)));
    changed.raw("RPM", 0, &8888.0f32.to_le_bytes());
    iracing.opener.set(MEMORY, changed.build());
    assert_eq!(iracing.native_channel("rpm"), Some(LegacyValue::Float(8888.0)));
    assert_eq!(iracing.simulator.data_valid.waits, 2, "getters never wait on the event");
}

#[test]
fn native_gates_none_and_acceleration_fallback_types() {
    let mut map = MapBuilder::new(3);
    map.boolean("IsOnTrackCar", true).float("Speed", 30.0).float("RPM", 5500.0).int("Gear", 4);
    let source = connected(&map);
    for name in ["max_rpm", "car_name", "lateral_velocity", "longitudinal_velocity", "vertical_velocity"] {
        assert_eq!(source.native_channel(name), Some(LegacyValue::None), "{name}");
    }
    assert_eq!(source.native_channel("lateral_acceleration"), Some(LegacyValue::Int(0)));
    assert_eq!(source.native_channel("vertical_acceleration"), Some(LegacyValue::Float(-GRAVITY)));
    assert_eq!(source.native_channel("track_loc"), Some(LegacyValue::Int(0)));
    let mut off = driving();
    off.raw("RPM", 0, &300.0f32.to_le_bytes()).raw("Speed", 0, &0.5f32.to_le_bytes());
    let source = connected(&off);
    assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(0)));
    assert_eq!(source.native_channel("gear"), Some(LegacyValue::Int(4)));
    off.raw("IsReplayPlaying", 0, &[1]);
    source.opener.set(MEMORY, off.build());
    for name in ["speed", "gear", "max_rpm", "car_name", "unknown"] {
        assert_eq!(source.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
    }
}

#[test]
fn native_session_values_keep_raw_names_selected_minimum_and_sequence_arithmetic() {
    let mut map = MapBuilder::new(3);
    map.boolean("IsOnTrackCar", true).float("Speed", 30.0).float("RPM", 5500.0).session(
        "---\nOilPress:\n 7\n\nManifoldPress:\n ab\n\nVelocityY:\n [1, 2]\n\nLatAccel:\n false\n\nLongAccel:\n 3x\n\nLFtempCM:\n 7\n\nLFtempCR:\n 7.0\n\nLFtempCL:\n 9\n\nBrakeABSactive:\n '01'\n\nDriverInfo:\n DriverCarIdx: 3\n DriverCarSLBlinkRPM: '07100'\n Drivers:\n - CarIdx: 3\n   CarScreenName: [1, 2]\n\nWeekendInfo:\n TrackDisplayShortName: false\n\n"
    );
    let source = connected(&map);
    for (name, expected) in [
        ("oil_pressure", LegacyValue::Int(700)),
        ("turbo_boost_pressure", LegacyValue::Str("ab".repeat(100))),
        ("lateral_velocity", LegacyValue::List(vec![])),
        ("lateral_acceleration", LegacyValue::Float(0.0)),
        ("longitudinal_acceleration", LegacyValue::Float(3.0)),
        ("tyre_temperature_lf", LegacyValue::Int(7)),
        ("abs_lock", LegacyValue::Int(1)),
        ("max_rpm", LegacyValue::Str("07100".into())),
        ("car_name", LegacyValue::List(vec![LegacyValue::Int(1), LegacyValue::Int(2)])),
        ("track_loc", LegacyValue::Bool(false)),
    ] {
        assert_eq!(source.native_channel(name), Some(expected), "{name}");
    }
}

/// Liveness: start-up needs the simulator status page and a header with version >= 1 and buffers.
#[test]
fn check_status_startup() {
    let map = driving();
    assert!(!listener(&map, false).is_running().unwrap());
    assert!(listener(&map, true).is_running().unwrap());
    let mut no_buffers = driving();
    no_buffers.header_field(32, 0);
    assert!(!listener(&no_buffers, true).is_running().unwrap());
    let mut version_zero = driving();
    version_zero.header_field(0, 0);
    assert!(!listener(&version_zero, true).is_running().unwrap());
    // A genuine mapping creation/access failure fails start-up and the liveness test, which
    // ends the dispatcher's loop.
    let denied = FakeBlocks::default();
    denied.fail_open(MEMORY.name, Some(std::io::ErrorKind::PermissionDenied));
    let error = Iracing::with_parts(denied, simulator(true, FakeEvent::signalling()), IracingOptions::default())
        .is_running()
        .unwrap_err();
    assert!(ends_dispatcher(&error));
}

#[test]
fn missing_read_only_mmap_is_zero_filled_and_retained_after_valid_startup_gates() {
    let pages = FakeBlocks::default();
    let mut source =
        Iracing::with_parts(pages.clone(), simulator(true, FakeEvent::signalling()), IracingOptions::default());
    assert!(!source.startup().expect("a missing named ACCESS_READ mmap is created, not an open failure"));
    assert_eq!(pages.held_count(MEMORY.name), 1);
    assert_eq!(pages.read_only_holds(), 1);
    assert!(!source.startup().unwrap());
    assert_eq!(pages.open_count(), 1, "startup retains the original zero-filled mapping");
    drop(source);
    assert_eq!(pages.held_count(MEMORY.name), 0);
    let mut listener =
        Iracing::with_parts(pages.clone(), simulator(true, FakeEvent::signalling()), IracingOptions::default());
    assert!(!listener.is_running().unwrap());
    assert_eq!(pages.held_count(MEMORY.name), 0, "listener checkStatus shuts down an uninitialized SDK");
}

/// Start-up: OpenEventW failing, or the data-valid wait timing out after 32 ms, makes start-up return
/// false (and the liveness test shut down) although the simulator reports running:1 and the memory is valid.
#[test]
fn startup_needs_the_data_valid_event() {
    let map = driving();
    for event in [FakeEvent::default(), FakeEvent { startup: Some(false), ..FakeEvent::default() }] {
        let pages = FakeBlocks::default();
        pages.set(MEMORY, map.build());
        let mut iracing = Iracing::with_parts(pages, simulator(true, event), IracingOptions::default());
        assert!(!iracing.is_running().unwrap());
        assert!(iracing.sdk.get_mut().unwrap().buffers.is_none());
        // The receive step without a header fails (the dispatcher never calls it after a failed liveness test).
        assert!(ends_dispatcher(&iracing.poll(&mut Telemetry::off()).unwrap_err()));
    }
}

/// The receive step waits once on the data-valid event before reading;
/// the liveness test while connected does not wait again.
#[test]
fn receive_data_waits_for_the_event() {
    let mut iracing = connected(&driving());
    assert!(iracing.is_running().unwrap());
    assert_eq!(iracing.simulator.data_valid.waits, 0);
    poll(&mut iracing);
    assert_eq!(iracing.simulator.data_valid.waits, 1);
}

#[test]
fn receive_timeout_still_copies_the_post_wait_frame_and_shutdown_forgets_the_event() {
    let pages = FakeBlocks::default();
    pages.set(MEMORY, driving().build());
    let mut source =
        Iracing::with_parts(pages.clone(), simulator(true, FakeEvent::signalling()), IracingOptions::default());
    assert!(source.is_running().unwrap());
    let mut replacement = driving();
    replacement.float("Speed", 47.0);
    let replacement = replacement.build();
    source.simulator.data_valid.timeout = true;
    source.simulator.data_valid.after_wait = Some(Box::new(move || pages.set(MEMORY, replacement.clone())));
    assert!(source.receive_native(&mut Telemetry::off()).unwrap());
    assert_eq!(source.native_channel("speed"), Some(LegacyValue::Float(47.0)));
    assert_eq!(source.simulator.data_valid.waits, 1);
    source.shutdown();
    assert_eq!(source.simulator.data_valid.forgets, 1);
    assert!(source.shared_mem.get_mut().unwrap().is_none());
}

/// Liveness: once connected, the previous speed and rpm are taken on every call (0 when missing).
#[test]
fn check_status_remembers_speed_and_rpm() {
    let mut iracing = connected(&driving());
    assert_eq!((iracing.previous.speed, iracing.previous.rpm), (Some(0.0), Some(0.0)));
    assert!(iracing.is_running().unwrap());
    assert_eq!((iracing.previous.speed, iracing.previous.rpm), (Some(30.0), Some(5500.0)));
}

/// Connected test: status 1 connects; otherwise the SessionNum workaround needs SessionNum missing, then present.
#[test]
fn connected_workaround_states() {
    let mut map = driving();
    map.header_field(4, 0);
    let mut iracing = listener(&map, true);
    assert!(!iracing.is_running().unwrap()); // state 1: SessionNum present
    let mut missing = MapBuilder::new(3);
    missing.header_field(4, 0).float("Speed", 1.0);
    iracing.opener.set(MEMORY, missing.build());
    assert!(!iracing.is_running().unwrap()); // state 2
    assert_eq!(iracing.sdk.get_mut().unwrap().workaround_state, 2);
    iracing.opener.set(MEMORY, map.build());
    assert!(iracing.is_running().unwrap()); // state 3 although status is 0
}

/// Channels: before and after the engine-off test, with their signs and conversions.
#[test]
fn get_channel_mapping() {
    let mut iracing = connected(&driving());
    let t = poll(&mut iracing);
    assert_eq!(t.speed, 30.0);
    assert_eq!(t.gear, 4.0);
    assert_eq!(t.max_rpm, 7400.0);
    assert_eq!(t.vehicle, "Mazda MX-5 Cup");
    assert_eq!(t.steering_wheel_angle, 3.0); // dams_front_tl_mode 0: LatAccel
    assert_eq!(t.rpm, 5500.0);
    assert_eq!(t.lateral_velocity, -1.5);
    assert_eq!(t.longitudinal_velocity, 29.5);
    assert_eq!(t.vertical_velocity, 29.5); // VelocityX
    assert_eq!(t.vertical_acceleration, f64::from(9.8f32) - 9.8);
    assert_eq!(t.lateral_acceleration, 3.0);
    assert_eq!(t.longitudinal_acceleration, -2.0);
    assert_eq!(t.yaw, -rad_to_deg(1.0));
    assert_eq!(t.pitch, -rad_to_deg(f64::from(0.1f32)));
    assert_eq!(t.roll, rad_to_deg(f64::from(-0.2f32)));
    assert_eq!(iracing.motion_rates().yaw_rate, Some(f64::from(0.3f32)));
    assert_eq!(t.location, "Okayama");
    assert_eq!(t.flag, 1.0);
    assert_eq!(t.raw_flag, 4.0);
    assert_eq!((t.spotter_left, t.spotter_right), (1.0, 1.0));
    assert_eq!(t.abs_lock, 1.0);
    assert_eq!(t.oil_pressure, 450.0);
    assert_eq!(t.turbo_boost_pressure, 125.0);
    assert_eq!(t.oil_temperature, 98.0);
    assert_eq!(t.lap, 7.0);
    assert_eq!(t.lap_dist_percent, 0.5);
    assert_eq!(t.fuel_level, 0.75);
    assert_eq!(t.water_temperature, 0.0);
    assert_eq!(t.tyre_pressure_lf, 0.0);
    assert_eq!(t.rumble_pitch_lf, 33.0);
}

/// Channels: suspension corners fall back to other corners, then 0.
#[test]
fn suspension_fallback_chains() {
    let t = poll(&mut connected(&driving()));
    assert_eq!(t.suspension_travel_lf, f64::from(0.05f32));
    assert_eq!(t.suspension_travel_lr, f64::from(0.07f32)); // LR missing, RR present
    assert_eq!(t.suspension_travel_rf, f64::from(0.05f32)); // RF missing, LF present
    assert_eq!(t.suspension_travel_rr, f64::from(0.07f32));
    assert_eq!(t.suspension_velocity_lf, f64::from(0.1f32));
    assert_eq!(t.suspension_velocity_rr, 0.0); // RR, LR, RF all missing
    assert_eq!(t.suspension_velocity_lr, f64::from(0.1f32)); // LR, RR missing, LF present
}

/// Channels: tyre temperature and wear are the minimum of the three readings; a missing reading fails (0).
#[test]
fn tyre_minimum() {
    let t = poll(&mut connected(&driving()));
    assert_eq!(t.tyre_temperature_lf, 78.0);
    assert_eq!(t.tyre_wear_rr, f64::from(0.85f32));
    assert_eq!(t.tyre_temperature_rr, 0.0);
}

/// The tyre minimum: the smaller of the first and third readings, then the smaller of the second and that
/// (which decides what a NaN reading does).
#[test]
fn tyre_minimum_nan_order() {
    let mut map = driving();
    map.raw("LFtempCM", 0, &f32::NAN.to_le_bytes());
    let t = poll(&mut connected(&map));
    assert_eq!(t.tyre_temperature_lf, 78.0); // NaN < 82 false -> 82; 78 < 82 -> 78
    let mut map = driving();
    map.raw("LFtempCR", 0, &f32::NAN.to_le_bytes());
    let t = poll(&mut connected(&map));
    assert_eq!(t.tyre_temperature_lf, 80.0);
}

/// Channels: the engine-off test zeroes the later channels; the earlier ones and the location are unaffected.
#[test]
fn engine_off_gate() {
    let mut map = driving();
    map.raw("RPM", 0, &300.0f32.to_le_bytes()).raw("Speed", 0, &0.5f32.to_le_bytes());
    let mut iracing = connected(&map);
    let t = poll(&mut iracing);
    assert_eq!(t.speed, f64::from(0.5f32));
    assert_eq!(t.gear, 4.0);
    assert_eq!(t.rpm, 0.0);
    assert_eq!(t.lateral_velocity, 0.0);
    assert_eq!(t.yaw, 0.0);
    assert_eq!(t.oil_pressure, 0.0);
    assert_eq!(t.location, ZERO_NAME);
    assert_eq!(t.vehicle, "Mazda MX-5 Cup");
    assert_eq!(iracing.motion_rates().yaw_rate, Some(0.0));
    let mut moving = driving();
    moving.raw("RPM", 0, &300.0f32.to_le_bytes()).raw("Speed", 0, &1.0f32.to_le_bytes());
    assert_eq!(poll(&mut connected(&moving)).rpm, 300.0);
}

/// Pause gate: replay or not in the car answers 0 everywhere while pausing is enabled.
#[test]
fn pause_preamble() {
    let mut replay = driving();
    replay.raw("IsReplayPlaying", 0, &[1]);
    let t = poll(&mut connected(&replay));
    assert_eq!(t.rpm, 0.0);
    assert_eq!(t.vehicle, ZERO_NAME);
    let mut garage = driving();
    garage.raw("IsOnTrackCar", 0, &[0]);
    assert_eq!(poll(&mut connected(&garage)).speed, 0.0);
    let mut iracing = connected(&garage);
    iracing.set_options(IracingOptions { pause_when_not_playing: false, ..IracingOptions::default() });
    assert_eq!(poll(&mut iracing).speed, 30.0);
}

/// Channels: SessionFlags priority order and normalized ids.
#[test]
fn flag_priority() {
    let cases = [
        (0u32, 0.0),
        (1024 | 4, 1.0),
        (32 | 2, 2.0),
        (2 | 16, 3.0),
        (16, 5.0),
        (8 | 1, 6.0),
        (1_048_576, 9.0),
        (65_536 | 1, 10.0),
        (1, 11.0),
        (4, 1.0),
        (256, 13.0),
        (1 << 30 | 1 << 29, 20.0),
        (1 << 29, 21.0),
        (1 << 31, 22.0),
        (128, 30.0),
        (131_072, 30.0),
        (64, 31.0),
        (512, 32.0),
        (32_768, 34.0),
        (524_288, 35.0),
        (16_384, 36.0),
        (1 << 28, 0.0),
    ];
    for (flags, expected) in cases {
        assert_eq!(normalized_flag(i64::from(flags)), expected, "flags {flags:#x}");
    }
}

/// Channels: spotter sets {2, 4, 6} left and {3, 4, 6} right; 5 matches neither.
#[test]
fn spotters() {
    for (value, left, right) in
        [(0i32, 0.0, 0.0), (2, 1.0, 0.0), (3, 0.0, 1.0), (4, 1.0, 1.0), (5, 0.0, 0.0), (6, 1.0, 1.0)]
    {
        let mut map = driving();
        map.raw("CarLeftRight", 0, &value.to_le_bytes());
        let t = poll(&mut connected(&map));
        assert_eq!((t.spotter_left, t.spotter_right), (left, right), "CarLeftRight {value}");
    }
}

/// Channels: steering_wheel_angle per dams_front_tl_mode; a missing value or bad mode stores the default 0.
#[test]
fn steering_by_dams_mode() {
    let mut iracing = connected(&driving());
    iracing.set_options(IracingOptions { dams_front_tl_mode: Some(2), ..IracingOptions::default() });
    assert_eq!(poll(&mut iracing).steering_wheel_angle, 0.5);
    iracing.set_options(IracingOptions { dams_front_tl_mode: None, ..IracingOptions::default() });
    assert_eq!(poll(&mut iracing).steering_wheel_angle, 0.0);
    let mut no_lat = MapBuilder::new(3);
    no_lat.boolean("IsOnTrackCar", true).int("SessionNum", 0).float("Speed", 30.0).float("RPM", 5000.0);
    let mut t = Telemetry::off();
    // The bare map leaves registry channels None, which ends the dispatcher after the channels are stored.
    assert!(ends_dispatcher(&connected(&no_lat).poll(&mut t).unwrap_err()));
    assert_eq!(t.steering_wheel_angle, 0.0);
}

/// Channels: yaw/pitch/roll fail when missing (the dispatcher stores 0); a missing `Speed` or `RPM` answers
/// `None`, which the dispatcher stores and then compares in its min/max update (an error that ends its main loop).
#[test]
fn missing_values() {
    let mut map = MapBuilder::new(3);
    map.boolean("IsOnTrackCar", true).int("SessionNum", 0);
    let mut iracing = connected(&map);
    let mut t = Telemetry::off();
    t.yaw = 5.0;
    t.speed = 9.0;
    t.rpm = 1000.0;
    t.abs_lock = 1.0;
    t.raw_flag = 3.0;
    assert!(ends_dispatcher(&iracing.poll(&mut t).unwrap_err()));
    assert_eq!(t.yaw, 0.0);
    assert_eq!(t.speed, 9.0);
    assert_eq!(t.rpm, 1000.0);
    assert_eq!(t.abs_lock, 0.0);
    assert_eq!(t.raw_flag, 3.0);
    assert_eq!(t.flag, 0.0); // Telemetry::off value kept: the bit test on a missing value failed, flag has no default
    assert_eq!(t.location, ZERO_NAME); // no WeekendInfo
    assert_eq!(t.vertical_acceleration, -9.8); // the acceleration of a missing variable is 0
}

/// Accelerations: with 360 Hz logging the variable is a list and element 0 is used.
#[test]
fn acceleration_list() {
    let mut map = driving();
    map.var("LatAccel6", FLOAT, 6);
    let mut list = MapBuilder::new(3);
    list.boolean("IsOnTrackCar", true).int("SessionNum", 0).float("Speed", 30.0).float("RPM", 5000.0);
    list.var("LongAccel", FLOAT, 6).raw("LongAccel", 0, &4.0f32.to_le_bytes()).raw(
        "LongAccel",
        1,
        &9.0f32.to_le_bytes(),
    );
    let mut t = Telemetry::off();
    assert!(ends_dispatcher(&connected(&list).poll(&mut t).unwrap_err()), "the bare map's None channels");
    assert_eq!(t.longitudinal_acceleration, 4.0);
}

/// car_name: '-1' when no driver has DriverCarIdx; nothing when DriverInfo is missing.
#[test]
fn car_name_lookup() {
    let mut map = driving();
    map.session(&SESSION.replace("DriverCarIdx: 3", "DriverCarIdx: 9"));
    assert_eq!(poll(&mut connected(&map)).vehicle, "-1");
    let mut bare = MapBuilder::new(3);
    bare.boolean("IsOnTrackCar", true).int("SessionNum", 0);
    let mut t = Telemetry::off();
    t.vehicle = "kept".into();
    assert!(ends_dispatcher(&connected(&bare).poll(&mut t).unwrap_err()));
    assert_eq!(t.vehicle, "kept");
}

/// wheel_slip_*: the slip estimate over the nested reads and the previous speed and rpm.
#[test]
fn wheel_slip_uses_previous_sample() {
    let mut iracing = connected(&driving());
    iracing.is_running().unwrap(); // previous = 30 m/s, 5500 rpm
    let mut faster = driving();
    faster.raw("Speed", 0, &31.0f32.to_le_bytes());
    iracing.opener.set(MEMORY, faster.build());
    let t = poll(&mut iracing);
    let inputs = SlipInputs {
        lateral_velocity: -1.5,
        speed: 31.0,
        rpm: 5500.0,
        gear: 4,
        previous_speed: 30.0,
        previous_rpm: 5500.0,
        throttle: 1.0,
        brake: 0.0,
    };
    assert_eq!(t.wheel_slip_lf, estimated_wheel_slip(WheelSide::Left, &inputs));
    assert_eq!(t.wheel_slip_rr, estimated_wheel_slip(WheelSide::Right, &inputs));
    assert!(t.wheel_slip_lf > 0.0);
}

/// The second-newest buffer is the one read.
#[test]
fn reads_second_newest_buffer() {
    let mut map = driving();
    let newest = super::memory::test_map::BUFFER_BASE + 2 * super::memory::test_map::BUFFER_LEN;
    // Speed is the 4th variable declared (64 bytes per variable in the test map).
    map.memory[newest + 3 * 64..newest + 3 * 64 + 4].copy_from_slice(&99.0f32.to_le_bytes());
    assert_eq!(poll(&mut connected(&map)).speed, 30.0);
}

/// car_name: the driver search ends at once for an empty list, mapping or string ('-1'); no value,
/// numbers and booleans cannot be iterated, and the keys or characters of a non-empty mapping or string have
/// no 'CarIdx', so those fail and the vehicle keeps its previous value.
#[test]
fn car_name_drivers_iteration() {
    let session = |drivers: &str| format!("---\nDriverInfo:\n DriverCarIdx: 3\n Drivers:{drivers}\n\n");
    let vehicle = |drivers: &str| {
        let mut map = driving();
        map.session(&session(drivers));
        let mut t = Telemetry::off();
        t.vehicle = "kept".into();
        assert!(connected(&map).poll(&mut t).unwrap());
        t.vehicle
    };
    for empty in [" []", " {}", " ''"] {
        assert_eq!(vehicle(empty), "-1", "Drivers:{empty}");
    }
    for not_iterable in ["", " ~", " 0", " 3", " 1.5", " true", " false", " abc", " {CarIdx: 3}"] {
        assert_eq!(vehicle(not_iterable), "kept", "Drivers:{not_iterable}");
    }
}

/// Liveness with one variable buffer: the start-up call succeeds (no variable is read), every channel of the
/// following poll fails (there is no second-newest buffer), and the next liveness test fails
/// at its `Speed` read, which ends the dispatcher's loop.
#[test]
fn single_buffer_ends_the_dispatcher_on_the_next_check() {
    let mut map = MapBuilder::new(1);
    map.boolean("IsOnTrackCar", true).int("SessionNum", 0).float("Speed", 30.0).float("RPM", 5500.0);
    let mut iracing = listener(&map, true);
    assert!(iracing.is_running().unwrap());
    let mut t = Telemetry::off();
    t.rpm = 1234.0;
    assert!(iracing.poll(&mut t).unwrap());
    assert_eq!(t.rpm, 1234.0, "the raising rpm channel keeps its value");
    assert!(ends_dispatcher(&iracing.is_running().unwrap_err()));
}

/// Liveness, connected branch: a failing `Speed` read (data outside the map) fails the liveness test, which
/// ends the dispatcher's loop.
#[test]
fn raising_previous_sample_ends_the_dispatcher() {
    let map = driving();
    let mut iracing = connected(&map);
    let mut broken = driving();
    // Speed is the 4th variable declared: move its data offset outside the memory.
    broken.header_field(super::memory::test_map::VAR_HEADERS + 3 * 144 + 4, i32::MAX);
    iracing.opener.set(MEMORY, broken.build());
    assert!(ends_dispatcher(&iracing.is_running().unwrap_err()));
}

/// Slip estimate: a NaN value fails the `value >= low` test and gives 0, so a
/// NaN lateral velocity contributes nothing.
#[test]
fn slip_with_nan_lateral_velocity() {
    let inputs = SlipInputs {
        lateral_velocity: f64::NAN,
        speed: 30.0,
        rpm: 6000.0,
        gear: 3,
        previous_speed: 25.0,
        previous_rpm: 5000.0,
        throttle: 0.0,
        brake: 0.3,
    };
    assert_eq!(estimated_wheel_slip(WheelSide::Left, &inputs), 0.0);
}

/// The dispatcher stores the `lateral_velocity` channel as it comes only in DAMS mode 0:
/// a missing `VelocityY` (`None`) ends its main
/// loop at the min/max update there; with every registry channel present nothing else answers `None`.
#[test]
fn none_lateral_velocity_ends_the_dispatcher_in_dams_mode_0() {
    let mut map = MapBuilder::new(3);
    map.boolean("IsOnTrackCar", true)
        .int("SessionNum", 0)
        .float("Speed", 30.0)
        .float("RPM", 5500.0)
        .int("Gear", 4)
        .float("VelocityX", 29.5)
        .float("VelocityZ", 0.25)
        .float("TireLF_RumblePitch", 0.0)
        .float("TireRF_RumblePitch", 0.0)
        .float("TireLR_RumblePitch", 0.0)
        .float("TireRR_RumblePitch", 0.0)
        .bits("SessionFlags", 0)
        .session(SESSION);
    let mut iracing = connected(&map);
    let mut t = Telemetry::off();
    t.lateral_velocity = 2.0;
    assert!(ends_dispatcher(&iracing.poll(&mut t).unwrap_err()));
    assert_eq!(t.lateral_velocity, 2.0);
    for mode in [Some(1), Some(2), None] {
        iracing.set_options(IracingOptions { dams_front_tl_mode: mode, ..IracingOptions::default() });
        assert!(iracing.poll(&mut t).unwrap(), "DAMS mode {mode:?}");
    }
}
