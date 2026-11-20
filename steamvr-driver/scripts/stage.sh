#!/bin/sh
# Build the Linux driver and stage the SteamVR driver folder (SteamVR/drivers/
# simracingstudio), with bin/linux64/driver_simracingstudio.so:
#   <stage>/simracingstudio/driver.vrdrivermanifest
#   <stage>/simracingstudio/resources/driver.vrresources
#   <stage>/simracingstudio/bin/linux64/driver_simracingstudio.so
# usage: stage.sh [stage-dir]   (default: $CARGO_TARGET_DIR/stage, CARGO_TARGET_DIR defaulting to
#        <repo>/target/bin/steamvr). Nothing is installed into SteamVR.
set -eu
here=$(dirname "$(realpath "$0")")
root=$(realpath "$here/..")
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(realpath -m "$root/../target/bin/steamvr")}"
stage=$(realpath -m "${1:-$CARGO_TARGET_DIR/stage}")
cargo build --release --manifest-path "$root/Cargo.toml"
rm -rf "$stage/simracingstudio"
mkdir -p "$stage/simracingstudio/bin/linux64" "$stage/simracingstudio/resources"
cp "$root/driver/simracingstudio/driver.vrdrivermanifest" "$stage/simracingstudio/"
cp "$root/driver/simracingstudio/resources/driver.vrresources" "$stage/simracingstudio/resources/"
cp "$CARGO_TARGET_DIR/release/libdriver_simracingstudio.so" "$stage/simracingstudio/bin/linux64/driver_simracingstudio.so"
find "$stage/simracingstudio" -type f | sort
