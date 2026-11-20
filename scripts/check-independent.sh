#!/usr/bin/env bash
# Host verification for the OBD2HUD firmware, game plugins and SteamVR tracker driver outside the root workspace.
# These run portable models and fixture harnesses, not live hardware. Windows-only harnesses
# are built/run by their per-project verification scripts. FS_MODS_ORIGINALS enables the live Lua/ZIP checks.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
target="${CARGO_TARGET_DIR:-$root/target}/independent-checks"
cargo_bin="${CARGO:-cargo}"
host=$(rustc -vV | sed -n 's/^host: //p')
[[ -n "$host" ]] || { echo "could not determine Rust host target" >&2; exit 1; }
command -v bwrap >/dev/null || { echo "bwrap is required for isolated host verification" >&2; exit 1; }
mkdir -p "$target"
target=$(realpath "$target")
check() {
    local name=$1 project=$2
    shift 2
    local unit_target="$target/$name"
    mkdir -p "$unit_target"
    echo "checking $project"
    CARGO_TARGET_DIR="$unit_target" bwrap --ro-bind / / --dev /dev --proc /proc \
        --tmpfs /tmp --tmpfs /dev/shm --bind "$unit_target" "$unit_target" --new-session \
        --setenv TMPDIR /tmp --setenv XDG_RUNTIME_DIR /tmp/openrigcue-independent-runtime \
        bash -c 'mkdir -p "$XDG_RUNTIME_DIR"; exec "$@"' bash \
        "$cargo_bin" test --manifest-path "$root/$project/Cargo.toml" --target "$host" --offline --locked "$@"
}
check rfactor1 game-plugins/rfactor1 -p rfactor1-smm
check rfactor2 game-plugins/rfactor2 -p rfactor2-smmp
check ets2 game-plugins/ets2 --workspace
check aerofly game-plugins/aerofly
check farming-simulator game-plugins/farming-simulator
check obd2hud firmware/obd2hud
check steamvr steamvr-driver
echo "independent host checks passed; target builds and differential target checks are separate"
