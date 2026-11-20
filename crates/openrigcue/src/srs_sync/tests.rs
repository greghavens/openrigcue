use super::*;
use std::collections::VecDeque;
use std::io::Cursor;
use zip::write::SimpleFileOptions;

struct FakeTransport {
    replies: VecDeque<Result<Vec<u8>>>,
    calls: Vec<BTreeMap<String, String>>,
}
impl FakeTransport {
    fn new(replies: Vec<Vec<u8>>) -> Self {
        Self { replies: replies.into_iter().map(Ok).collect(), calls: Vec::new() }
    }
}
impl Transport for FakeTransport {
    fn post(&mut self, fields: &[(&str, &str)], limit: u64) -> Result<Vec<u8>> {
        self.calls.push(fields.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect());
        let bytes = self.replies.pop_front().expect("unexpected HTTP request")?;
        if bytes.len() as u64 > limit {
            bail!("fake body exceeds request limit");
        }
        Ok(bytes)
    }
}
fn target() -> Target {
    Target { game: "ac".into(), page: Page::Motion, vehicle: Vehicle::Game(0) }
}
fn archive(game: &str, vehicle: Value) -> Vec<u8> {
    zip_bytes(
        "tune.json",
        &serde_json::to_vec(&serde_json::json!({
            "game": game, "vehicle": vehicle, "srs_version":"2026.6.14.2", "motion_effects":{"roll":3}
        }))
        .unwrap(),
    )
}
fn zip_bytes(name: &str, content: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(name, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
    zip.write_all(content).unwrap();
    zip.finish().unwrap().into_inner()
}
fn listing(version: u32) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"resource^author/unsafe":{"current_version":version,"alias":"Driver"}}))
        .unwrap()
}
fn library() -> (tempfile::TempDir, Manifest) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("profiles")).unwrap();
    (dir, Manifest { targets: vec![target()], ..Manifest::default() })
}
fn args() -> SyncArgs {
    SyncArgs {
        list: false,
        key_file: None,
        library_dir: None,
        game: vec![],
        vehicle: vec![],
        page: vec![],
        targets: None,
        all_games: false,
        refresh: false,
    }
}

#[test]
fn incremental_sync_uses_full_resource_ids_and_skips_unchanged_versions() {
    let (dir, mut manifest) = library();
    let bytes = archive("ac", serde_json::json!(0));
    let mut transport = FakeTransport::new(vec![listing(1), bytes.clone(), listing(1), listing(2), bytes.clone()]);
    let mut output = Vec::new();
    assert_eq!(sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut output).unwrap().downloaded, 1);
    let record = manifest.records.values().next().unwrap();
    assert_eq!(record.resource_id, "resource^author/unsafe");
    assert_eq!(fs::read(dir.path().join(&record.file)).unwrap(), bytes);
    assert!(read_profile_file(&bytes).is_ok());
    assert!(!record.file.contains("author"));
    assert_eq!(transport.calls[0]["action"], "list_resources");
    assert_eq!(transport.calls[0]["page"], "motion");
    assert_eq!(transport.calls[1]["resource_id"], "resource^author/unsafe");
    assert_eq!(transport.calls[1]["vehicle"], "0");
    assert_eq!(transport.calls[1]["version"], "1");
    assert_eq!(sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut output).unwrap().unchanged, 1);
    assert_eq!(transport.calls.len(), 3);
    assert_eq!(sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut output).unwrap().downloaded, 1);
    assert_eq!(transport.calls[4]["version"], "2");
    assert_eq!(read_manifest(dir.path()).unwrap().records.len(), 1);
    assert!(!String::from_utf8(output).unwrap().contains("secret"));
    assert!(!String::from_utf8(fs::read(dir.path().join("manifest.json")).unwrap()).unwrap().contains("secret"));
}

#[test]
fn edited_copies_and_prior_versions_are_preserved_without_active_database_writes() {
    let (dir, mut manifest) = library();
    fs::write(dir.path().join("profile.db"), b"active database marker").unwrap();
    let original = archive("ac", serde_json::json!(0));
    let mut transport = FakeTransport::new(vec![listing(1), original.clone(), listing(1), listing(2), original]);
    sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    let old = manifest.records.values().next().unwrap().file.clone();
    fs::write(dir.path().join(&old), b"local user edit").unwrap();
    let counts = sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    assert_eq!(counts.preserved, 1);
    assert_eq!(transport.calls.len(), 3);
    sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    let new = &manifest.records.values().next().unwrap().file;
    assert_ne!(&old, new);
    assert_eq!(fs::read(dir.path().join(old)).unwrap(), b"local user edit");
    assert_eq!(fs::read(dir.path().join("profile.db")).unwrap(), b"active database marker");
}

#[test]
fn missing_files_and_explicit_refresh_download_again_and_keep_content_identity() {
    let (dir, mut manifest) = library();
    let bytes = archive("ac", serde_json::json!(0));
    let mut transport =
        FakeTransport::new(vec![listing(1), bytes.clone(), listing(1), bytes.clone(), listing(1), bytes]);
    sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    let first = manifest.records.values().next().unwrap().file.clone();
    fs::remove_file(dir.path().join(&first)).unwrap();
    assert_eq!(
        sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap().downloaded,
        1
    );
    assert_eq!(manifest.records.values().next().unwrap().file, first);
    assert_eq!(sync(&mut transport, dir.path(), "secret", &mut manifest, true, &mut Vec::new()).unwrap().downloaded, 1);
    assert_eq!(fs::read_dir(dir.path().join("profiles")).unwrap().count(), 1);
}

#[test]
fn explicit_targets_persist_and_default_covers_only_known_game_level_queries() {
    let first = choose_targets(&args(), &[]).unwrap();
    assert_eq!(first.len(), Catalog::builtin().games().len() * 5);
    assert!(first.iter().all(|target| target.vehicle == Vehicle::Game(0)));
    let mut selected = args();
    selected.game = vec!["ac".into()];
    selected.page = vec![Page::Motion];
    selected.vehicle = vec!["Ferrari / exact name".into()];
    let selected = choose_targets(&selected, &[]).unwrap();
    assert_eq!(selected.len(), 2);
    assert_eq!(choose_targets(&args(), &selected).unwrap(), selected);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("targets.json");
    fs::write(&path, br#"[{"game":"gt7","page":"belt","vehicle":0}]"#).unwrap();
    let mut imported = args();
    imported.targets = Some(path);
    let result = choose_targets(&imported, &selected).unwrap();
    assert_eq!(result.len(), 3);
    imported.game = vec!["not-a-game".into()];
    assert!(choose_targets(&imported, &[]).is_err());
}

#[test]
fn download_validation_rejects_unsafe_paths_bombs_and_wrong_metadata() {
    let target = target();
    for bytes in [
        b"not zip".to_vec(),
        zip_bytes("../escape.json", br#"{}"#),
        zip_bytes("/absolute.json", br#"{}"#),
        zip_bytes("windows\\escape.json", br#"{}"#),
        zip_bytes("tune.json", br#"{"game":"ac","vehicle":0}"#),
        archive("gt7", serde_json::json!(0)),
        archive("ac", serde_json::json!(["invalid vehicle"])),
        zip_bytes("tune.json", &vec![b' '; MAX_UNPACKED as usize + 1]),
    ] {
        assert!(validate_archive(&bytes, &target).is_err());
    }
    let mut named = target;
    named.vehicle = Vehicle::Named("Exact car".into());
    assert!(validate_archive(&archive("ac", serde_json::json!("Exact car")), &named).is_ok());
    assert!(bounded_read(Cursor::new(vec![0; 5]), 4).is_err());
}

#[test]
fn query_vehicle_and_archive_origin_remain_separate_through_incremental_sync() {
    let (dir, mut manifest) = library();
    let origin = archive("ac", serde_json::json!("Named origin car"));
    let mut transport = FakeTransport::new(vec![listing(1), origin, listing(1)]);
    let counts = sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    assert_eq!(counts.downloaded, 1);
    assert_eq!(transport.calls[1]["vehicle"], "0");
    assert_eq!(manifest.records.values().next().unwrap().target.vehicle, Vehicle::Game(0));
    let entries = openrigcue_config::synced_profiles::list(dir.path()).unwrap();
    assert_eq!(entries[0].target.vehicle, openrigcue_config::synced_profiles::Vehicle::Game(0));
    assert_eq!(
        entries[0].source_vehicle,
        Some(openrigcue_config::synced_profiles::Vehicle::Named("Named origin car".into()))
    );
    let (_, content) = openrigcue_config::synced_profiles::load(dir.path(), &entries[0].id).unwrap();
    assert_eq!(content.item("vehicle").unwrap().as_str(), Some("Named origin car"));
    let mut output = Vec::new();
    list_cached(dir.path(), &mut output).unwrap();
    assert!(String::from_utf8(output).unwrap().contains("source vehicle \"Named origin car\""));
    assert_eq!(sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap().unchanged, 1);
    let mut named = target();
    named.vehicle = Vehicle::Named("Destination car".into());
    assert!(validate_archive(&archive("ac", serde_json::json!(0)), &named).is_ok());
    assert!(validate_archive(&archive("ac", serde_json::json!("Other origin")), &named).is_ok());
    assert!(validate_archive(&archive("acc", serde_json::json!("Other origin")), &named).is_err());
}

#[test]
fn failed_remote_target_retains_successful_archive_and_manifest_without_leaking_response() {
    let (dir, mut manifest) = library();
    let mut second = target();
    second.page = Page::Belt;
    manifest.targets.push(second);
    let mut transport =
        FakeTransport::new(vec![listing(1), archive("ac", serde_json::json!(0)), b"secret invalid response".to_vec()]);
    let mut output = Vec::new();
    let counts = sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut output).unwrap();
    assert_eq!(counts, Counts { downloaded: 1, failures: 1, ..Counts::default() });
    assert_eq!(read_manifest(dir.path()).unwrap().records.len(), 1);
    assert!(!String::from_utf8(output).unwrap().contains("secret"));
}

#[test]
fn key_storage_is_private_nonclobbering_and_never_echoes_the_key() {
    assert!(validate_key(b" ").is_err());
    assert!(validate_key(b"line\nbreak").is_err());
    assert_eq!(validate_key(b" sample-key\n").unwrap(), "sample-key");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("premium.key");
    atomic_write(&path, b"sample-key", true).unwrap();
    assert_eq!(read_key(&path).unwrap(), "sample-key");
    assert!(atomic_write(&path, b"other-key", true).is_err());
    assert_eq!(read_key(&path).unwrap(), "sample-key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key(&path).is_err());
        let link = dir.path().join("key-link");
        symlink(&path, &link).unwrap();
        assert!(read_key(&link).is_err());
    }
}

#[test]
fn library_lock_and_manifest_paths_prevent_concurrent_or_outside_writes() {
    let (dir, mut manifest) = library();
    let guard = lock_library(dir.path()).unwrap();
    assert!(lock_library(dir.path()).is_err());
    drop(guard);
    assert!(lock_library(dir.path()).is_ok());
    let mut transport = FakeTransport::new(vec![listing(1), archive("ac", serde_json::json!(0))]);
    sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    manifest.records.values_mut().next().unwrap().file = "../outside.srs".into();
    save_manifest(dir.path(), &manifest).unwrap();
    assert!(read_manifest(dir.path()).is_err());
}

#[test]
fn invalid_download_does_not_replace_the_previous_version_or_leave_partial_files() {
    let (dir, mut manifest) = library();
    let mut transport = FakeTransport::new(vec![
        listing(1),
        archive("ac", serde_json::json!(0)),
        listing(2),
        b"secret malformed download".to_vec(),
    ]);
    sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    let before = fs::read(dir.path().join("manifest.json")).unwrap();
    let mut output = Vec::new();
    let counts = sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut output).unwrap();
    assert_eq!(counts.failures, 1);
    assert_eq!(fs::read(dir.path().join("manifest.json")).unwrap(), before);
    assert_eq!(fs::read_dir(dir.path().join("profiles")).unwrap().count(), 1);
    assert!(!String::from_utf8(output).unwrap().contains("secret"));
    assert_eq!(manifest.records.values().next().unwrap().remote_version, serde_json::json!(1));
}

#[test]
fn cli_has_no_key_value_argument_and_parses_exact_vehicle_targets() {
    use clap::Parser;
    assert!(crate::Cli::try_parse_from(["openrigcue", "srs-sync", "--key", "secret"]).is_err());
    let cli = crate::Cli::try_parse_from([
        "openrigcue",
        "srs-sync",
        "--game",
        "ac",
        "--vehicle",
        "Exact car",
        "--page",
        "motion",
    ])
    .unwrap();
    let crate::Command::SrsSync(args) = cli.command else { panic!("wrong command") };
    assert_eq!(args.vehicle, ["Exact car"]);
    assert_eq!(args.page, [Page::Motion]);
    let help = crate::Cli::try_parse_from(["openrigcue", "srs-key", "--help"]).err().unwrap();
    assert!(help.to_string().contains("--stdin"));
}

#[test]
fn authentication_and_repeated_bad_listings_stop_before_the_remaining_targets() {
    for authentication in [true, false] {
        let (dir, mut manifest) = library();
        manifest.targets = choose_targets(&args(), &[]).unwrap();
        let replies = if authentication {
            VecDeque::from([Err(AuthRejected.into())])
        } else {
            (0..3).map(|_| Ok(b"invalid key response".to_vec())).collect()
        };
        let mut transport = FakeTransport { replies, calls: Vec::new() };
        let mut output = Vec::new();
        let counts = sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut output).unwrap();
        assert_eq!(transport.calls.len(), if authentication { 1 } else { 3 });
        assert_eq!(counts.failures, transport.calls.len());
        assert_eq!(read_manifest(dir.path()).unwrap().targets.len(), Catalog::builtin().games().len() * 5);
        assert!(String::from_utf8(output).unwrap().contains("Stopped after"));
    }
}

#[test]
fn cached_listing_requires_no_key_or_network_and_shows_author_version_and_import_path() {
    let (dir, mut manifest) = library();
    let mut transport = FakeTransport::new(vec![listing(3), archive("ac", serde_json::json!(0))]);
    sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    let mut output = Vec::new();
    list_cached(dir.path(), &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("ac / 0 / motion"));
    assert!(output.contains("author \"Driver\" | version 3 | cached"));
    assert!(output.contains(dir.path().join(&manifest.records.values().next().unwrap().file).to_str().unwrap()));
    let mut args = args();
    args.list = true;
    args.library_dir = Some(dir.path().into());
    args.key_file = Some(dir.path().join("missing-key"));
    run(args).unwrap();
    assert_eq!(transport.calls.len(), 2);
}

#[test]
fn persistent_sync_log_records_failed_resource_and_keeps_success_without_secrets() {
    let (dir, mut manifest) = library();
    let mut second = target();
    second.page = Page::Belt;
    manifest.targets.push(second);
    let mut transport = FakeTransport {
        replies: VecDeque::from([
            Ok(listing(3)),
            Ok(archive("ac", serde_json::json!(0))),
            Ok(listing(4)),
            Err(anyhow::anyhow!("credential-123 RAW RESPONSE AND REQUEST")),
        ]),
        calls: Vec::new(),
    };
    let mut output = Vec::new();
    let counts = sync(&mut transport, dir.path(), "credential-123", &mut manifest, false, &mut output).unwrap();
    assert_eq!(counts.downloaded, 1);
    assert_eq!(counts.failures, 1);
    assert_eq!(read_manifest(dir.path()).unwrap().records.len(), 1);
    let paths: Vec<_> = fs::read_dir(dir.path().join("logs")).unwrap().map(|entry| entry.unwrap().path()).collect();
    assert_eq!(paths.len(), 1);
    let text = fs::read_to_string(&paths[0]).unwrap();
    let rows: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let failure = rows.iter().find(|row| row["event"] == "target_failed").unwrap();
    assert_eq!(failure["target"]["page"], "belt");
    assert_eq!(failure["resource_id"], "resource^author/unsafe");
    assert_eq!(failure["version"], "4");
    assert_eq!(failure["stage"], "download_request");
    assert!(rows.iter().any(|row| row["event"] == "downloaded"));
    assert_eq!(rows.last().unwrap()["event"], "run_finished");
    assert!(!text.contains("credential-123"));
    assert!(!text.contains("RAW RESPONSE"));
    assert!(String::from_utf8(output).unwrap().contains(paths[0].to_str().unwrap()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&paths[0]).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn repeated_sync_logs_are_distinct_and_cached_browsing_never_creates_one() {
    let (dir, mut manifest) = library();
    let mut transport = FakeTransport::new(vec![b"{}".to_vec(), b"{}".to_vec()]);
    list_cached(dir.path(), &mut Vec::new()).unwrap();
    assert!(!dir.path().join("logs").exists());
    for _ in 0..2 {
        sync(&mut transport, dir.path(), "credential", &mut manifest, false, &mut Vec::new()).unwrap();
    }
    assert_eq!(fs::read_dir(dir.path().join("logs")).unwrap().count(), 2);
}

#[test]
fn persistent_sync_log_keeps_http_code_and_redacts_remote_identifier_echoes() {
    let (dir, mut manifest) = library();
    manifest.targets.push(Target { game: "acc".into(), ..target() });
    let mut transport = FakeTransport {
        replies: VecDeque::from([
            Ok(br#"{"resource-credential-123":{"current_version":"v-credential-123"}}"#.to_vec()),
            Err(RequestFailure { category: "authentication", status: Some(403) }.into()),
        ]),
        calls: Vec::new(),
    };
    let counts = sync(&mut transport, dir.path(), "credential-123", &mut manifest, false, &mut Vec::new()).unwrap();
    assert_eq!(counts.failures, 1);
    assert_eq!(transport.calls.len(), 2, "authentication stops before the next target");
    let path = fs::read_dir(dir.path().join("logs")).unwrap().next().unwrap().unwrap().path();
    let text = fs::read_to_string(path).unwrap();
    assert!(!text.contains("credential-123"));
    let rows: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let failure = rows.iter().find(|row| row["event"] == "target_failed").unwrap();
    assert_eq!(failure["category"], "authentication");
    assert_eq!(failure["http_status"], 403);
    assert_eq!(failure["resource_id"], "resource-[redacted]");
    assert_eq!(failure["version"], "v-[redacted]");
}

/// Alias with `' [PRO]'` for a truthy `protuner`, and the
/// page's equipment description, are what the picker shows for each listing row.
#[test]
fn listing_alias_and_equipment_description_follow_the_original_picker_columns() {
    let row = serde_json::json!({
        "alias": "Driver", "protuner": true, "current_version": 1,
        "motion_manufacturer": "DOF REALITY", "motion_model": "H3",
        "shakers_profile": "SRS ShakeKit or SRS Shakebox",
        "belt_manufacturer": "PT-ACTUATOR", "belt_model": "TENSIONR",
        "rumble_model": "Seat + Pedals", "haptic_model": "HAPTICR"
    });
    assert_eq!(listing_alias(&row), "Driver [PRO]");
    for (page, expected) in [
        (Page::Motion, "DOF REALITY [H3]"),
        (Page::Shakers, "SRS ShakeKit or SRS Shakebox"),
        (Page::Belt, "PT-ACTUATOR [TENSIONR]"),
        (Page::Rumble, "SIM3D [Seat + Pedals]"),
        (Page::Haptic, "HAPTIC [HAPTICR]"),
    ] {
        assert_eq!(listing_description(page, &row), expected, "{page:?}");
    }
    for protuner in [serde_json::json!(false), serde_json::json!(0), Value::Null, serde_json::json!("")] {
        assert_eq!(listing_alias(&serde_json::json!({"alias": "Driver", "protuner": protuner})), "Driver");
    }
    assert_eq!(listing_alias(&serde_json::json!({"alias": "Driver"})), "Driver");
    // An incomplete or malformed row keeps its download; only the label is empty.
    assert_eq!(listing_description(Page::Motion, &serde_json::json!({"motion_manufacturer": "DOF REALITY"})), "");
    assert_eq!(listing_description(Page::Rumble, &serde_json::json!({"rumble_model": 4})), "");
    assert_eq!(listing_alias(&serde_json::json!({"alias": "Line\nbreak\u{7}"})), "Linebreak");
}

#[test]
fn sync_records_listing_labels_and_refreshes_them_for_unchanged_downloads() {
    let (dir, mut manifest) = library();
    let bytes = archive("ac", serde_json::json!(0));
    let row = |alias: &str, protuner: bool, model: &str| {
        serde_json::to_vec(&serde_json::json!({"resource": {"current_version": 1, "alias": alias,
            "protuner": protuner, "motion_manufacturer": "DOF REALITY", "motion_model": model}}))
        .unwrap()
    };
    // An older library: the same record without a stored description.
    let mut transport = FakeTransport::new(vec![listing(1), bytes.clone()]);
    manifest.targets = vec![target()];
    sync(&mut transport, dir.path(), "secret", &mut manifest, false, &mut Vec::new()).unwrap();
    let (identity, old) = manifest.records.iter().next().map(|(id, record)| (id.clone(), record.file.clone())).unwrap();
    assert_eq!(manifest.records[&identity].description, "");
    assert!(!String::from_utf8(fs::read(dir.path().join("manifest.json")).unwrap()).unwrap().contains("description"));

    let mut library = Manifest { targets: vec![target()], ..Manifest::default() };
    let mut transport = FakeTransport::new(vec![row("Driver", true, "H3"), bytes.clone(), row("Renamed", false, "H3")]);
    let scratch = tempfile::tempdir().unwrap();
    fs::create_dir(scratch.path().join("profiles")).unwrap();
    let counts = sync(&mut transport, scratch.path(), "secret", &mut library, false, &mut Vec::new()).unwrap();
    assert_eq!(counts.downloaded, 1);
    let record = library.records.values().next().unwrap();
    assert_eq!((record.alias.as_str(), record.description.as_str()), ("Driver [PRO]", "DOF REALITY [H3]"));
    let counts = sync(&mut transport, scratch.path(), "secret", &mut library, false, &mut Vec::new()).unwrap();
    assert_eq!((counts.downloaded, counts.unchanged), (0, 1));
    assert_eq!(transport.calls.len(), 3, "an unchanged version is relabelled without another download");
    let saved = read_manifest(scratch.path()).unwrap();
    let record = saved.records.values().next().unwrap();
    assert_eq!((record.alias.as_str(), record.description.as_str()), ("Renamed", "DOF REALITY [H3]"));
    assert_eq!(fs::read(scratch.path().join(&record.file)).unwrap(), bytes);
    let mut listed = Vec::new();
    list_cached(scratch.path(), &mut listed).unwrap();
    let listed = String::from_utf8(listed).unwrap();
    assert!(listed.contains("author \"Renamed\" | version 1 | cached | equipment \"DOF REALITY [H3]\""), "{listed}");
    assert!(dir.path().join(old).exists());
}
