//! Boundary regressions for the consumers of live, dynamically typed values.
use crate::views::{config_integer, Sources};
use crate::*;
use num_bigint::BigInt;
use num_traits::{One, ToPrimitive};
use openrigcue_model::{
    native_object::NativeObject,
    native_value::{Dict, LegacyValue},
    Telemetry,
};
use std::{
    borrow::Cow,
    sync::Mutex,
    time::{Duration, Instant},
};

#[test]
fn native_rates_cap_before_float_conversion_and_keep_real_overflow_boundary() {
    let mut motion = Dict::new();
    motion.insert("refresh_rate", LegacyValue::from_integer(BigInt::one() << 2048usize));
    let mut config = Dict::new();
    config.insert("MOTIONBOX", LegacyValue::Dict(motion));
    let config = NativeConfig(PyValue::from(&LegacyValue::Dict(config)));
    assert_eq!(loop_period_from_config(&config, HostOs::Linux).unwrap().as_nanos(), 33_333_334);
    assert_eq!(
        loop_period_from_config(&config, HostOs::Windows).unwrap_err().to_string(),
        "int too large to convert to float"
    );
    assert_eq!(test_period_from_config(false, &config, HostOs::Linux).unwrap().as_nanos(), 142_857_143);
}
#[test]
fn motion_argument_capture_observes_each_read_and_missing_unused_argument() {
    let mut calls = Vec::new();
    let error = capture_motion_arguments(|name| {
        calls.push(name);
        (name != "landing_gear_nose").then_some(LegacyValue::Int(calls.len() as i64))
    })
    .unwrap_err();
    assert_eq!(calls.last(), Some(&"landing_gear_nose"));
    assert_eq!(error.to_string(), "'landing_gear_nose'");
    assert!(error.traceback().contains("line 437"));
    let logged = error.refresh_motionbox_log();
    assert_eq!(logged.matches("in backend.controllers.controllers.refresh_motionbox").count(), 1);
    assert!(logged.contains("line 437"));
    assert!(!logged.contains("line 418"));
    let mut counter = 0;
    let data = capture_motion_arguments(|_| {
        counter += 1;
        Some(LegacyValue::Int(counter))
    })
    .unwrap();
    assert_eq!(data.native_value("roll"), Some(&LegacyValue::Int(1)));
    assert_eq!(data.native_value("pitch"), Some(&LegacyValue::Int(2)));
    assert_eq!(data.native_value("front_tl"), Some(&LegacyValue::Int(28)));
}
#[test]
fn mutable_config_aliases_are_read_without_detaching_the_dictionary() {
    let mut section = LegacyValue::Object(NativeObject::new(LegacyValue::Dict(Dict::new())));
    section.assign_item("refresh_rate", LegacyValue::Int(30)).unwrap();
    let mut root = Dict::new();
    root.insert("MOTIONBOX", section.clone());
    let config = NativeConfig(PyValue::from(&LegacyValue::Object(NativeObject::new(LegacyValue::Dict(root)))));
    assert_eq!(config_integer(&config, "MOTIONBOX", "refresh_rate"), Ok(PyValue::Int(30)));
    let wide = BigInt::one() << 300usize;
    section.assign_item("refresh_rate", LegacyValue::from_integer(wide.clone())).unwrap();
    assert_eq!(config_integer(&config, "MOTIONBOX", "refresh_rate").unwrap().to_native().integer(), Some(wide));
}
#[test]
fn nan_preparation_keeps_prefix_writes_before_native_integer_overflow() {
    let mut telemetry = Telemetry::unfilled();
    telemetry.set_native("roll", LegacyValue::Float(f64::NAN));
    telemetry.set_native("pitch", LegacyValue::from_integer(BigInt::one() << 2048usize));
    telemetry.set_native("yaw", LegacyValue::Float(f64::NAN));
    let error = clear_telemetry_nan_checked(&mut telemetry).unwrap_err();
    assert_eq!(error.exception(), Exception::OverflowError);
    assert_eq!(error.to_string(), "int too large to convert to float");
    assert_eq!(telemetry.native_value("roll"), Some(&LegacyValue::Int(0)));
    assert!(telemetry.yaw.is_nan());
}
#[test]
fn arbitrary_final_positions_are_retained_until_the_device_consumer() {
    let platform = builtin("dof-reality-p3").unwrap().unwrap();
    let mut cueing = MotionCueing::new(platform, ClipFactors::default());
    let settings = MotionSettings::default();
    let tune = MotionTune { max_power: 1.0e200, ..MotionTune::default() };
    let context = StreamContext {
        settings: &settings,
        tune: &tune,
        listener: Listener::Game,
        game: "ac",
        previous_actual_game: "",
        game_kind: GameKind::Racing,
    };
    let positions = cueing.compute_native(&Telemetry::off(), &context, Instant::now()).unwrap().unwrap();
    assert!(positions.iter().any(|value| value.integer().unwrap().to_i64().is_none()));
    cueing.record_native_delivered(&positions);
    assert!(cueing.compute_native(&Telemetry::off(), &context, Instant::now() + Duration::from_millis(20)).is_ok());
}
#[test]
fn raw_surge_type_reaches_the_original_uncaught_weight_comparison() {
    let platform = builtin("dof-reality-p3").unwrap().unwrap();
    let mut cueing = MotionCueing::new(platform, ClipFactors::default());
    let settings = MotionSettings { legacy_gear_smoothing: true, ..MotionSettings::default() };
    let tune = MotionTune::default();
    let context = StreamContext {
        settings: &settings,
        tune: &tune,
        listener: Listener::Game,
        game: "ac",
        previous_actual_game: "",
        game_kind: GameKind::Racing,
    };
    let mut telemetry = Telemetry::off();
    telemetry.set_native("longitudinal_acceleration", LegacyValue::Str("1".into()));
    let error = cueing.compute_native(&telemetry, &context, Instant::now()).unwrap_err();
    assert_eq!(error.to_string(), "'>=' not supported between instances of 'str' and 'int'");
    assert!(error.traceback().contains("line 381"));
}
struct SwitchingModel {
    settings: MotionSettings,
    models: Mutex<Vec<&'static str>>,
}
impl ConfigView for SwitchingModel {
    fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing> {
        self.settings.value(section, key)
    }
    fn current_model(&self) -> Option<Result<PyValue, CueError>> {
        Some(Ok(PyValue::from(self.models.lock().unwrap().remove(0))))
    }
}
#[test]
fn max_power_rereads_model_between_main_actuators_and_center() {
    let config =
        SwitchingModel { settings: MotionSettings::default(), models: Mutex::new(vec!["P3", "T1000-3M PRS200"]) };
    let platform = builtin("dof-reality-p3").unwrap().unwrap();
    let tune = MotionTune { max_power: 50.0, ..MotionTune::default() };
    let sources = Sources { config: &config, game: &tune, manufacturer: "DOF REALITY", game_running: "ac" };
    assert_eq!(crate::shaping::scale_travel(5_000.0, 0, &platform, &sources), Ok(18_884.0));
    assert!(config.models.lock().unwrap().is_empty());
}
#[test]
fn native_list_index_conversion_has_the_source_index_sized_error() {
    let platform = builtin("dof-reality-p3").unwrap().unwrap();
    for value in [PyValue::Int(i128::MAX), PyValue::from(&LegacyValue::from_integer(BigInt::one() << 2048usize))] {
        let error = platform.table(&value).unwrap_err();
        assert_eq!(error.exception(), Exception::IndexError);
        assert_eq!(error.to_string(), "cannot fit 'int' into an index-sized integer");
    }
}
#[test]
fn wide_random_interval_retains_saved_python_draws_and_word_order() {
    let shift = BigInt::one() << 1024usize;
    let mut previous = None;
    let mut rng = DefaultRng::seeded(0);
    for line in include_str!("../tests/data/random-wide-reference.txt").lines() {
        let values: Vec<i128> = line.split_whitespace().map(|v| v.parse().unwrap()).collect();
        let seed = values[0] as u64;
        if previous != Some(seed) {
            rng = DefaultRng::seeded(seed);
            previous = Some(seed);
        }
        let low = &shift + BigInt::from(values[1]);
        let high = &shift + BigInt::from(values[2]);
        assert_eq!(rng.randint_native(&low, &high), &shift + BigInt::from(values[3]), "saved native draw {line}");
    }
}

#[test]
fn unused_unbounded_boost_factor_is_not_converted_before_the_boost_branch() {
    let platform = builtin("dof-reality-p3").unwrap().unwrap();
    let settings = MotionSettings::default();
    let mut tune = crate::views::testing::raw_game(&MotionTune::default());
    crate::views::testing::set_game(
        &mut tune,
        &["motion_boost_factor"],
        Some(PyValue::from(&LegacyValue::from_integer(-(BigInt::one() << 2048usize)))),
    );
    let sources = Sources { config: &settings, game: &tune, manufacturer: "DOF REALITY", game_running: "ac" };
    assert_eq!(crate::shaping::shape_rotary_native(2500.0, None, &platform, &sources), Ok(LegacyValue::Int(2500)));
    let previous = LegacyValue::Int(2000);
    let error = crate::shaping::shape_rotary_native(2500.0, Some(&previous), &platform, &sources).unwrap_err();
    assert_eq!(error.to_string(), "int too large to convert to float");
    assert!(error.traceback().contains("line 647"));
}
#[test]
fn negative_smoothing_window_skips_unreached_native_gear_division() {
    let settings = MotionSettings::default();
    let mut tune = crate::views::testing::raw_game(&MotionTune::default());
    crate::views::testing::set_game(&mut tune, &["motion_effect_smoothing_factor", "surge"], Some(PyValue::Int(-1)));
    let context = crate::smoothing::WindowContext {
        sources: Sources { config: &settings, game: &tune, manufacturer: "DOF REALITY", game_running: "ac" },
        soft_start_engaged: false,
        gear_smoothing: Some(PyValue::from(&LegacyValue::from_integer(BigInt::one() << 2048usize))),
        speed: PyValue::Float(10.0),
    };
    assert_eq!(crate::smoothing::smoothing_window_with_workers(Axis::Surge, &context, None), Ok(PyValue::Int(-1)));
}

#[test]
fn live_test_sliders_read_in_source_order_and_publish_the_captured_heave() {
    struct Inputs {
        settings: MotionSettings,
        reads: Mutex<Vec<usize>>,
    }
    impl ConfigView for Inputs {
        fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing> {
            self.settings.value(section, key)
        }
        fn current_test_value(&self, index: usize) -> Option<PyValue> {
            let mut reads = self.reads.lock().unwrap();
            reads.push(index);
            Some(PyValue::Int(reads.len() as i128 * 10))
        }
    }
    let input = Inputs { settings: MotionSettings::default(), reads: Mutex::new(vec![]) };
    let tune = MotionTune::default();
    let context = TestContext { settings: &input, tune: &tune, game: "none" };
    let cueing = MotionCueing::new(builtin("dof-reality-p3").unwrap().unwrap(), ClipFactors::default());
    let (_, heave) = cueing.test_native_frame(&std::array::from_fn(|_| PyValue::Int(0)), &context).unwrap().unwrap();
    assert_eq!(*input.reads.lock().unwrap(), [1, 0, 2, 3, 4, 5, 6, 7]);
    assert_eq!(heave, 6.0);
}

#[test]
fn stateless_test_computation_does_not_wait_for_a_blocked_stream_provider() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    struct Blocked {
        settings: MotionSettings,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        first: AtomicBool,
    }
    impl ConfigView for Blocked {
        fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing> {
            if section == "CUSTOM_FEATURES"
                && key == "enable_legacy_gear_smoothing"
                && self.first.swap(false, Ordering::SeqCst)
            {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            }
            self.settings.value(section, key)
        }
    }
    let state =
        Arc::new(Mutex::new(MotionCueing::new(builtin("dof-reality-p3").unwrap().unwrap(), ClipFactors::default())));
    let test = state.lock().unwrap().test_snapshot();
    let (entered, waiting) = mpsc::channel();
    let (resume, released) = mpsc::channel();
    let (stream, done) = (state.clone(), mpsc::channel());
    let worker = std::thread::spawn(move || {
        let provider = Blocked {
            settings: MotionSettings::default(),
            entered,
            resume: Mutex::new(released),
            first: AtomicBool::new(true),
        };
        let tune = MotionTune::default();
        let context = StreamContext {
            settings: &provider,
            tune: &tune,
            listener: Listener::Game,
            game: "ac",
            previous_actual_game: "",
            game_kind: GameKind::Racing,
        };
        stream.lock().unwrap().compute_native(&Telemetry::off(), &context, Instant::now()).unwrap();
    });
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let tester = std::thread::spawn(move || {
        let (settings, tune) = (MotionSettings::default(), MotionTune::default());
        let context = TestContext { settings: &settings, tune: &tune, game: "none" };
        done.0
            .send(test.test_native_frame(&std::array::from_fn(|_| PyValue::Int(0)), &context).unwrap().is_some())
            .unwrap();
    });
    let result = done.1.recv_timeout(Duration::from_secs(1));
    resume.send(()).unwrap();
    worker.join().unwrap();
    tester.join().unwrap();
    assert_eq!(result, Ok(true));
}

#[test]
#[ignore = "bounded optimized live-input throughput measurement"]
fn native_live_frame_budget() {
    struct Live {
        config: NativeConfig,
    }
    impl ConfigView for Live {
        fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing> {
            self.config.value(section, key)
        }
        fn read_value(&self, section: &str, key: &str) -> Result<PyValue, CueError> {
            self.config.read_value(section, key)
        }
        fn current_model(&self) -> Option<Result<PyValue, CueError>> {
            Some(self.read_value("MOTIONBOX", "model"))
        }
    }
    for (id, key) in [("dof-reality-p3", "P3"), ("dof-reality-h6", "H6")] {
        let settings =
            Live { config: crate::views::testing::native_config(&MotionSettings::default(), "DOF REALITY", key) };
        let tune = crate::views::testing::raw_game(&MotionTune::default());
        let tune = RawGameConfig::live(&tune.0.to_native());
        let context = StreamContext {
            settings: &settings,
            tune: &tune,
            listener: Listener::Game,
            game: "ac",
            previous_actual_game: "",
            game_kind: GameKind::Racing,
        };
        let mut cueing = MotionCueing::new(builtin(id).unwrap().unwrap(), ClipFactors::default());
        let mut telemetry = Telemetry::off();
        for (key, value) in [
            ("roll", 0.5),
            ("pitch", 0.2),
            ("yaw", 1.0),
            ("longitudinal_acceleration", 0.5),
            ("vertical_acceleration", 0.1),
            ("speed", 11.0),
        ] {
            telemetry.set_native(key, LegacyValue::Float(value));
        }
        let now = Instant::now();
        for i in 0..25 {
            let values = cueing.compute_native(&telemetry, &context, now + Duration::from_millis(i)).unwrap().unwrap();
            cueing.record_native_delivered(&values);
        }
        let started = Instant::now();
        for i in 0..100 {
            let values =
                cueing.compute_native(&telemetry, &context, now + Duration::from_millis(25 + i)).unwrap().unwrap();
            cueing.record_native_delivered(&values);
        }
        eprintln!(
            "native frame budget {id}: frames=100 elapsed_us={} mean_us={:.3}",
            started.elapsed().as_micros(),
            started.elapsed().as_secs_f64() * 10_000.0
        );
    }
}

#[test]
fn motion_test_external_weights_clamp_before_native_integer_output() {
    let path = std::path::Path::new("private-test-mixing.csv");
    let direction = format!("h\n{}", (0..12).map(|_| "0,+,+,+,+,+,+,+,+\n").collect::<String>());
    let deadzone = format!("h\n{}", (0..12).map(|_| "0,0,0,0,0,0,0,0,0\n").collect::<String>());
    let huge = "10000000000000000000000000000000000000000";
    let percentages = format!(
        "h\n{}",
        (0..12).map(|_| format!("0,{huge},{huge},{huge},{huge},{huge},{huge},{huge},{huge}\n")).collect::<String>()
    );
    let ext = crate::ExternalMixing::from_files(
        Some((path, direction.as_bytes())),
        Some((path, deadzone.as_bytes())),
        Some((path, percentages.as_bytes())),
    )
    .unwrap()
    .unwrap();
    let config = std::sync::Arc::new(crate::MotionConfig::builtin().clone().with_external_mixing(&ext));
    let snapshot =
        crate::MotionTestSnapshot::new(config.platform("P3").transpose().unwrap(), ClipFactors::default(), config);
    let settings = MotionSettings::default();
    let tune = MotionTune { enhanced_gforces: false, ..MotionTune::default() };
    let context = TestContext { settings: &settings, tune: &tune, game: "ac" };
    let mut sliders = std::array::from_fn(|_| PyValue::Int(0));
    sliders[Axis::Roll.index()] = PyValue::Int(100);
    let (positions, _) = snapshot.test_native_values_frame(&sliders, &context).unwrap().unwrap();
    let expected = [3891, 204, 3891, 3891, 204, 3891, 3891, 3891, 3891, 3891, 3891, 3891];
    assert_eq!(positions, expected.map(LegacyValue::Int));
    assert_eq!(snapshot.test_native_frame(&sliders, &context).unwrap().unwrap().0, expected);
}
