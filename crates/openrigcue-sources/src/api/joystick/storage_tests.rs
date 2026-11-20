use super::*;
use openrigcue_config::import::{sqlitedict, value::LegacyValue};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const DEVICE: &str = "Fixture pad";

fn stored(trigger: i64) -> LegacyValue {
    serde_json::json!({"right":{"joystick":DEVICE,"type":"axis","trigger":trigger,"mode":"Single Axis","invert_axis":false}}).into()
}

#[test]
fn canonical_lookup_runs_only_during_enabled_mapping_and_errors_end_that_thread() {
    let enabled = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(Ok(Some(stored(0)))));
    let inputs = JoystickInputs::default();
    inputs.set_axis(DEVICE, 0, 32767);
    let mut source = JoystickSource::stepped(JoystickProfile::default(), JoystickOptions::default(), &inputs);
    let flag = enabled.clone();
    let count = reads.clone();
    let row = current.clone();
    source.set_runtime_reader(JoystickRuntimeReader::with_profile_lookup(
        move || JoystickRuntime { enabled: flag.load(Ordering::SeqCst), name: "Default".into(), profile: None },
        move |name| {
            assert_eq!(name, "Default");
            count.fetch_add(1, Ordering::SeqCst);
            lock(&row).clone()
        },
    ));
    source.step();
    assert!(!source.is_running().unwrap());
    source.poll(&mut Telemetry::off()).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    enabled.store(true, Ordering::SeqCst);
    assert!(source.is_running().unwrap());
    source.poll(&mut Telemetry::off()).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 0, "status/receiveData must not read the dictionary");
    source.step();
    let mut telemetry = Telemetry::off();
    source.poll(&mut telemetry).unwrap();
    assert_eq!(telemetry.roll, 10.0);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    enabled.store(false, Ordering::SeqCst);
    *lock(&current) = Err("fixture SQLite error".into());
    source.step();
    assert!(!source.mapping_ended(), "disabled mapping never reaches the dictionary error");
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    enabled.store(true, Ordering::SeqCst);
    source.step();
    assert!(source.mapping_ended());
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    source.poll(&mut telemetry).unwrap();
    assert_eq!(telemetry.roll, 10.0, "failed lookup leaves prior deflections untouched");
    *lock(&current) = Ok(Some(stored(0)));
    source.step();
    assert_eq!(reads.load(Ordering::SeqCst), 2, "editing the row does not restart the ended mapping thread");
    assert!(source.is_running().unwrap(), "checkStatus still follows the enabled global");
}

#[test]
fn canonical_mapping_reads_live_sqlite_row_and_stops_on_selected_decode_error() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("metadata.db");
    let table = Arc::new(sqlitedict::AutocommitTable::open(&path, "joystick").unwrap());
    table.set("Default", &stored(0)).unwrap();
    let inputs = JoystickInputs::default();
    inputs.set_axis(DEVICE, 0, 32767);
    inputs.set_axis(DEVICE, 1, -32768);
    let mut source = JoystickSource::stepped(JoystickProfile::default(), JoystickOptions::default(), &inputs);
    let database = table.clone();
    source.set_runtime_reader(JoystickRuntimeReader::with_profile_lookup(
        || JoystickRuntime { enabled: true, name: "Default".into(), profile: None },
        move |name| database.get(name).map_err(|error| error.to_string()),
    ));
    source.step();
    let mut telemetry = Telemetry::off();
    source.poll(&mut telemetry).unwrap();
    assert_eq!(telemetry.roll, 10.0);
    let fixture = sqlitedict::PendingTable::open(&path, "joystick").unwrap();
    fixture.execute_batch("INSERT INTO joystick (key,value) VALUES ('opaque',X'ff')").unwrap();
    sqlitedict::set_item(&path, "joystick", "Default", &stored(1)).unwrap();
    source.step();
    source.poll(&mut telemetry).unwrap();
    assert_eq!(telemetry.roll, 0.0, "mapping uses the external current row");
    assert!(!source.mapping_ended(), "unrelated opaque row remains unread");
    fixture.execute_batch("REPLACE INTO joystick (key,value) VALUES ('Default',X'ff')").unwrap();
    source.step();
    assert!(source.mapping_ended());
    sqlitedict::set_item(&path, "joystick", "Default", &stored(0)).unwrap();
    source.step();
    source.poll(&mut telemetry).unwrap();
    assert_eq!(telemetry.roll, 0.0, "row repair leaves the ended thread's previous deflections");
}
