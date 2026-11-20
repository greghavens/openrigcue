//! `scripts/openrigcue-bridge-run`: the bridge is inserted after Proton's verb, everything else is kept.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/openrigcue-bridge-run");

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("openrigcue-bridge-wrapper-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in for Proton (or a native game) that prints one argument per line.
fn fake_program(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn run(args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    let mut cmd = Command::new(SCRIPT);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect()
}

#[test]
fn bridge_goes_between_proton_verb_and_game() {
    let dir = scratch("proton");
    let proton_dir = dir.join("Proton 10.0");
    fs::create_dir_all(&proton_dir).unwrap();
    let proton = fake_program(&proton_dir, "proton");
    let bridge = dir.join("openrigcue-bridge.exe");
    fs::write(&bridge, b"MZ").unwrap();
    let proton = proton.to_str().unwrap();
    let bridge = bridge.to_str().unwrap();

    let printed =
        run(&[proton, "waitforexitandrun", "/games/My Game/acs.exe", "-x"], &[("OPENRIGCUE_BRIDGE_EXE", bridge)]);
    assert_eq!(printed, ["waitforexitandrun", bridge, "--", "/games/My Game/acs.exe", "-x"]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn non_proton_commands_run_unchanged() {
    let dir = scratch("native");
    let game = fake_program(&dir, "game");
    let bridge = dir.join("openrigcue-bridge.exe");
    fs::write(&bridge, b"MZ").unwrap();
    let printed = run(&[game.to_str().unwrap(), "run", "a b"], &[("OPENRIGCUE_BRIDGE_EXE", bridge.to_str().unwrap())]);
    assert_eq!(printed, ["run", "a b"]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_bridge_runs_the_game_unchanged() {
    let dir = scratch("missing");
    let proton = fake_program(&dir, "proton");
    let printed = run(
        &[proton.to_str().unwrap(), "run", "game.exe"],
        &[("OPENRIGCUE_BRIDGE_EXE", "/nonexistent/openrigcue-bridge.exe")],
    );
    assert_eq!(printed, ["run", "game.exe"]);
    fs::remove_dir_all(dir).unwrap();
}
