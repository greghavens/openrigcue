#!/usr/bin/env bash
# Build the OBD2HUD firmware flash image (Arduino/IDF 3.3 flash layout) from the Rust application code.
# Nothing is flashed.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$here/../../target/bin/fw-obd2hud}"
. "$here/../tools/esp32-build-environment.sh"
runtime=$(bash "$here/../tools/esp32-runtime.sh")
export OPENRIGCUE_IDF33_ROOT="$runtime"
cd "$here"
"${CARGO:-$HOME/.cargo/bin/cargo}" +esp build --release -p obd2hud-firmware
rel="$CARGO_TARGET_DIR/xtensa-esp32-espidf/release"
out="$CARGO_TARGET_DIR/image"
mkdir -p "$out"
"$ESPFLASH" partition-table --skip-update-check --to-binary --output "$out/obd2hud.partitions.bin" partitions.csv
cp "$runtime/esp32-1.0.6/tools/partitions/boot_app0.bin" "$out/boot_app0.bin"
cp "$runtime/esp32-1.0.6/tools/sdk/bin/bootloader_dio_40m.bin" "$out/bootloader.bin"
"$ESPFLASH" save-image --skip-update-check --chip esp32 --flash-mode dio --flash-freq 40mhz --flash-size 4mb \
  --partition-table partitions.csv "$rel/obd2hud-firmware" "$out/firmware.bin"
node "$here/../tools/esp32-image.js" "$out/bootloader.bin" "$out/firmware.bin" \
  "$out/obd2hud.partitions.bin" "$out/boot_app0.bin" "$out/obd2hud-merged.bin"
printf 'Rust obd2hud firmware with pinned IDF3.3 runtime: %s\n' "$out"
