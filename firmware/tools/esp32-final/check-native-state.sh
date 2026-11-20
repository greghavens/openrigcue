#!/usr/bin/env bash
# Build and run the host check of the frame and handshake storage.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
out=${1:?Usage: check-native-state.sh PRIVATE_OUTPUT}
mkdir -p "$out"
out=$(realpath "$out")
cargo=${CARGO:-$HOME/.cargo/bin/cargo}
rustc=${RUSTC:-$HOME/.cargo/bin/rustc}
CARGO_TARGET_DIR="$out/belt" "$cargo" +stable build --manifest-path "$root/firmware/belt/Cargo.toml" --target x86_64-unknown-linux-gnu --lib --locked --offline
CARGO_TARGET_DIR="$out/obd" "$cargo" +stable build --manifest-path "$root/firmware/obd2hud/Cargo.toml" -p obd2hud-core --target x86_64-unknown-linux-gnu --lib --locked --offline
"$rustc" +stable --edition=2021 -Awarnings -O "$root/firmware/tools/esp32-final/native-state.rs" --extern belt_fw="$out/belt/x86_64-unknown-linux-gnu/debug/libbelt_fw.rlib" --extern obd2hud_core="$out/obd/x86_64-unknown-linux-gnu/debug/libobd2hud_core.rlib" -L dependency="$out/belt/x86_64-unknown-linux-gnu/debug/deps" -L dependency="$out/obd/x86_64-unknown-linux-gnu/debug/deps" -o "$out/native-rust"
${CC:-cc} -std=c11 -O2 -Wall -Wextra "$root/firmware/tools/esp32-final/native-state.c" -o "$out/native-c"
"$out/native-rust" > "$out/rust.txt"
"$out/native-c" > "$out/c.txt"
cmp "$out/rust.txt" "$out/c.txt"
cat "$out/rust.txt"
