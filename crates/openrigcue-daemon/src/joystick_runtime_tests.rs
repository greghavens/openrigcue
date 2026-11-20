use super::*;
use openrigcue_config::import::{sqlitedict, value::LegacyValue};
use openrigcue_model::Telemetry;
use openrigcue_sources::api::joystick::{JoystickOptions, JoystickProfile, JoystickSource, JoystickTiming};
use openrigcue_sources::Source;
use std::time::{Duration, Instant};

#[test]
fn production_feature_reader_retains_each_current_native_scalar() {
    let state = ServiceState::new(
        ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
        OriginalPlatform::Windows,
    );
    let reader = joystick_runtime_reader(state.clone(), Shared::new("Default".into()));
    for (key, value) in [
        ("sway_when_roll_joy", Some(LegacyValue::Str("Y".into()))),
        ("surge_when_pitch_joy", Some(LegacyValue::Str("no".into()))),
        ("sway_when_roll_joy", Some(LegacyValue::None)),
        ("surge_when_pitch_joy", Some(LegacyValue::Bool(true))),
        ("sway_when_roll_joy", None),
    ] {
        state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                let fields = raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap();
                match &value {
                    Some(value) => {
                        fields.insert(key, value.clone());
                    }
                    None => {
                        fields.remove(key);
                    }
                }
            });
        });
        assert_eq!(reader.read_feature(key), Some(value));
    }
}

#[test]
fn production_mapping_reader_uses_current_sqlite_and_keeps_thread_failure_local() {
    const DEVICE: &str = "Fixture pad";
    let temporary = tempfile::tempdir().unwrap();
    let directory = ConfigDir::at(temporary.path().join("config"));
    let store = ConfigStore::load(directory.clone()).unwrap();
    store.save().unwrap();
    let state = ServiceState::new(store, OriginalPlatform::Windows);
    state.controls.write().switches.joystick = true;
    let profile = |trigger| {
        LegacyValue::from(
            serde_json::json!({"right":{"joystick":DEVICE,"type":"axis","trigger":trigger,"mode":"Single Axis","invert_axis":false}}),
        )
    };
    let path = directory.metadata_file();
    sqlitedict::set_item(&path, "joystick", "Default", &profile(0)).unwrap();
    let inputs = JoystickInputs::default();
    inputs.set_axis(DEVICE, 0, 32767);
    inputs.set_axis(DEVICE, 1, -32768);
    let timing = JoystickTiming {
        input_pass: Duration::from_millis(1),
        mapping_pass: Duration::from_millis(1),
        mapping_idle: Duration::from_millis(1),
    };
    let mut source =
        JoystickSource::with_timing(JoystickProfile::default(), JoystickOptions::default(), inputs, timing);
    source.set_runtime_reader(joystick_runtime_reader(state.clone(), Shared::new("Default".into())));
    let wait_roll = |source: &mut JoystickSource, wanted| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut telemetry = Telemetry::off();
            source.poll(&mut telemetry).unwrap();
            if telemetry.roll == wanted {
                break;
            }
            assert!(Instant::now() < deadline, "mapping did not reach {wanted}");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    wait_roll(&mut source, 10.0);
    // The cached table still holds the first Default entry.
    sqlitedict::set_item(&path, "joystick", "Default", &profile(1)).unwrap();
    wait_roll(&mut source, 0.0);
    let fixture = sqlitedict::PendingTable::open(&path, "joystick").unwrap();
    fixture.execute_batch("REPLACE INTO joystick (key,value) VALUES ('Default',X'ff')").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !source.mapping_ended() {
        assert!(Instant::now() < deadline, "selected pickle failure did not end threadJoy");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(source.is_running().unwrap(), "listener status still follows the global enabled switch");
    let mut telemetry = Telemetry::off();
    source.poll(&mut telemetry).unwrap();
    assert_eq!(telemetry.roll, 0.0);
    sqlitedict::set_item(&path, "joystick", "Default", &profile(0)).unwrap();
    source.poll(&mut telemetry).unwrap();
    assert_eq!(telemetry.roll, 0.0);
    assert!(source.mapping_ended());
}
