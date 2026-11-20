#!/usr/bin/env bash
# Copy the three built VR products into a package directory. Registers and installs nothing on the host.
# Usage: stage.sh DEST
set -euo pipefail
[[ $# == 1 ]] || { echo "usage: $0 DEST" >&2; exit 2; }
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
target="${CARGO_TARGET_DIR:-$root/target}"
tracker_target="${OPENRIGCUE_TRACKER_TARGET_DIR:-$target/bin/steamvr}"
dest=$1

native="$dest/SteamVR/drivers/openrigcue"
original="$dest/SteamVR/drivers/simracingstudio"
install -Dm0644 "$root/crates/openrigcue-steamvr-driver/driver/openrigcue/driver.vrdrivermanifest" "$native/driver.vrdrivermanifest"
install -Dm0644 "$root/crates/openrigcue-steamvr-driver/driver/openrigcue/resources/settings/default.vrsettings" "$native/resources/settings/default.vrsettings"
install -Dm0644 "$root/crates/openrigcue-steamvr-driver/driver/openrigcue/resources/steamvr-driver.example.toml" "$native/resources/steamvr-driver.example.toml"
install -Dm0755 "$target/release/libopenrigcue_steamvr_driver.so" "$native/bin/linux64/driver_openrigcue.so"
install -Dm0644 "$root/steamvr-driver/driver/simracingstudio/driver.vrdrivermanifest" "$original/driver.vrdrivermanifest"
install -Dm0644 "$root/steamvr-driver/driver/simracingstudio/resources/driver.vrresources" "$original/resources/driver.vrresources"
install -Dm0755 "$tracker_target/release/libdriver_simracingstudio.so" "$original/bin/linux64/driver_simracingstudio.so"
install -Dm0755 "$target/release/libopenrigcue_xr_layer.so" "$dest/OpenXR/libopenrigcue_xr_layer.so"
install -Dm0644 "$root/crates/openrigcue-xr-layer/manifest/openrigcue_xr_layer.json.in" "$dest/OpenXR/openrigcue_xr_layer.json.in"
install -Dm0644 "$root/packaging/vr/README.md" "$dest/README.md"
