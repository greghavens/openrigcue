use super::*;
use crate::import::first_run;

/// Every factory tune converts to the typed schema; spot values are pinned.
#[test]
fn factory_tunes_match_master_config() {
    let factory = FactoryTunes::builtin();
    assert_eq!(factory.games().count(), 126);
    assert!(factory.get("condor3").is_some(), "in master_config, not in the catalog");
    let ac = factory.get("ac").unwrap();
    assert_eq!(ac.gear_shift_perc, 90.0);
    assert_eq!(ac.default_max_speed_kmh, 150.0);
    assert_eq!(ac.motion_boost_factor, 5.0);
    assert_eq!(ac.gear_smoothing_factor, 95.0);
    assert_eq!(ac.wind_type, WindType::Fan);
    assert_eq!(ac.shaker_engine_type, "E");
    assert_eq!(ac.intellitune_motion_preference.as_deref(), Some(""));
    assert_eq!(ac.max_telemetry_vars.yaw, 1800.0);
    assert_eq!(ac.max_telemetry_vars.surge_acceleration, 18.0);
    assert_eq!(ac.max_telemetry_vars.pitch_rate, None);
    assert_eq!(ac.motion_effects.sway, 15.0);
    assert_eq!(ac.motion_effects.engine, 1.0);
    assert!(!ac.motion_switch.engine);
    assert_eq!(ac.shaker_volume.sub, 6.0);
    assert_eq!(ac.shaker_effects.road_bump, 6.0);
    assert_eq!(ac.shaker_cutoff.engine_fadeoff_start, 95.0);
    assert_eq!(ac.shaker_resonance.engine, 35.0);
    assert_eq!(ac.rumble_effects.rpm_start_fading, 70.0);
    assert_eq!(ac.haptic_effects.rpm_start_fading, 94.0);
    assert!(ac.vehicles.is_empty());

    let il2 = factory.get("il2").unwrap();
    assert_eq!(il2.motion_effects.pitch_type, Some(AxisSource::Angle));
    assert_eq!(il2.max_telemetry_vars.roll_rate, Some(60.0));

    let api = factory.get("api").unwrap();
    let vehicles: Vec<&str> = api.vehicles.keys().map(String::as_str).collect();
    assert_eq!(vehicles, ["Epic Roller Coasters", "Flashout 3", "Fly Dangerous", "Torque Drift"]);

    let redrover = factory.get("redrover").unwrap();
    assert_eq!(redrover.game.as_deref(), Some("redrover"));
    assert_eq!(redrover.vehicle, Some(ExportedVehicle::Game(0)));
    assert_eq!(redrover.srs_version.as_deref(), Some("2.13.0.0"));
    assert!(redrover.shaker_pitchshift.is_some());
}

/// The typed tune writes back the same profile dict (numbers compare numerically).
#[test]
fn tune_json_round_trip() {
    for (game, tune) in FactoryTunes::builtin().games() {
        let json = tune.to_json();
        let back = Tune::from_legacy(&json.into()).unwrap();
        assert_eq!(&back, tune, "{game}");
    }
}

/// The expert profiles: five tuner sets; jeffweaver published the profiles v153 imports.
#[test]
fn expert_profiles() {
    let experts = ExpertProfiles::builtin();
    let aliases: Vec<&str> = experts.0.keys().map(String::as_str).collect();
    assert_eq!(aliases, ["giba", "globespy", "jeffweaver", "marcelo", "yves"]);
    let jeff = experts.tuner("jeffweaver").unwrap();
    assert_eq!(jeff.name, "Jeff Weaver");
    assert_eq!(jeff.motion_equipment, "DOF Reality P6");
    assert_eq!(jeff.shakers_equipment, "Elite");
    assert_eq!(jeff.wind_equipment, "2 Fans");
    assert_eq!(jeff.games.len(), 87);
    assert_eq!(jeff.games.values().filter(|g| g.publish).count(), 23);
    assert!(jeff.games["ac"].publish);
    assert_eq!(jeff.games["ac"].max_telemetry_vars.yaw, 1800.0);
    let published_vehicles: usize =
        jeff.games.values().map(|g| g.vehicles.values().filter(|v| v.publish).count()).sum();
    assert_eq!(published_vehicles, 142);
}

/// The first-run layouts convert to typed speaker layouts.
#[test]
fn first_run_layouts_are_typed() {
    let layouts = &first_run().shaker_layouts;
    assert_eq!(layouts.names().count(), 9);
    let ushake = layouts.get("SRS UShake6").unwrap();
    assert_eq!(ushake.center.sfx.engine, 4.0);
    assert!(ushake.center.enable);
    let enhanced = layouts.get("SRS UShake6 Enhanced").unwrap();
    assert_eq!(enhanced.center.sfx.engine, 7.0);
}
