#!/usr/bin/env bash
# Builds the OpenRigCue SteamVR driver, stages it as a SteamVR driver folder and registers that
# folder with SteamVR (vrpathreg adddriver). Restart SteamVR afterwards.
#
#   scripts/install.sh [--dry-run] [--no-build] [--zig] [--prefix DIR]
#
#   --dry-run   print what would be done, change nothing
#   --no-build  use the existing release build
#   --zig       build with cargo-zigbuild against glibc 2.31 (the SteamLinuxRuntime "sniper"
#               baseline) instead of the host glibc
#   --prefix    where to stage the driver folder
#               (default: ${XDG_DATA_HOME:-~/.local/share}/openrigcue/steamvr-driver)
#
# Environment: STEAMVR (SteamVR install dir), CARGO_TARGET_DIR (as for cargo).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate="$(dirname "$here")"
workspace="$(cd "$crate/../.." && pwd)"
dry_run=0
build=1
zig=0
prefix="${XDG_DATA_HOME:-$HOME/.local/share}/openrigcue/steamvr-driver"

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1 ;;
        --no-build) build=0 ;;
        --zig) zig=1 ;;
        --prefix) prefix="$2"; shift ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

run() {
    echo "+ $*"
    if [ "$dry_run" = 0 ]; then "$@"; fi
}

find_steamvr() {
    if [ -n "${STEAMVR:-}" ]; then echo "$STEAMVR"; return; fi
    for d in "$HOME/.local/share/Steam/steamapps/common/SteamVR" \
             "$HOME/.steam/steam/steamapps/common/SteamVR" \
             "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common/SteamVR"; do
        if [ -x "$d/bin/vrpathreg.sh" ]; then echo "$d"; return; fi
    done
    return 1
}

steamvr="$(find_steamvr)" || { echo "SteamVR not found; set STEAMVR=/path/to/SteamVR" >&2; exit 1; }
vrpathreg="$steamvr/bin/vrpathreg.sh"
[ -x "$vrpathreg" ] || { echo "$vrpathreg not found" >&2; exit 1; }

target_dir="${CARGO_TARGET_DIR:-$workspace/target}"
if [ "$zig" = 1 ]; then
    triple="x86_64-unknown-linux-gnu"
    so="$target_dir/$triple/release/libopenrigcue_steamvr_driver.so"
    if [ "$build" = 1 ]; then
        command -v cargo-zigbuild >/dev/null || { echo "cargo-zigbuild not installed (cargo install cargo-zigbuild; needs zig)" >&2; exit 1; }
        run cargo zigbuild --manifest-path "$workspace/Cargo.toml" --release -p openrigcue-steamvr-driver --target "$triple.2.31"
    fi
else
    so="$target_dir/release/libopenrigcue_steamvr_driver.so"
    if [ "$build" = 1 ]; then
        run cargo build --manifest-path "$workspace/Cargo.toml" --release -p openrigcue-steamvr-driver
    fi
fi
if [ "$dry_run" = 0 ] && [ ! -f "$so" ]; then echo "$so not built" >&2; exit 1; fi

# vrserver runs inside SteamLinuxRuntime_sniper (glibc 2.31). pressure-vessel normally provides the
# host's newer glibc in the container, but report what the library needs so a failure is explicable.
if [ -f "$so" ] && command -v objdump >/dev/null; then
    need="$(objdump -T "$so" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
    host="$(getconf GNU_LIBC_VERSION 2>/dev/null || echo unknown)"
    echo "driver needs up to $need; host has $host; the sniper runtime ships glibc 2.31"
    if ! objdump -T "$so" | grep -q ' HmdDriverFactory$'; then
        echo "error: $so does not export HmdDriverFactory" >&2; exit 1
    fi
fi

dest="$prefix/openrigcue"
run mkdir -p "$dest/bin/linux64" "$dest/resources/settings"
run install -m 0644 "$crate/driver/openrigcue/driver.vrdrivermanifest" "$dest/driver.vrdrivermanifest"
run install -m 0644 "$crate/driver/openrigcue/resources/settings/default.vrsettings" "$dest/resources/settings/default.vrsettings"
run install -m 0644 "$crate/driver/openrigcue/resources/steamvr-driver.example.toml" "$dest/resources/steamvr-driver.example.toml"
run install -m 0755 "$so" "$dest/bin/linux64/driver_openrigcue.so"

config="${XDG_CONFIG_HOME:-$HOME/.config}/openrigcue/steamvr-driver.toml"
if [ ! -e "$config" ]; then
    run mkdir -p "$(dirname "$config")"
    run install -m 0644 "$crate/driver/openrigcue/resources/steamvr-driver.example.toml" "$config"
fi

run "$vrpathreg" adddriver "$dest"
echo
if [ "$dry_run" = 1 ]; then echo "(dry run: nothing was changed)"; fi
echo "Installed to $dest and registered with SteamVR. Restart SteamVR."
echo "Check: \"$vrpathreg\" show   (lists external drivers)"
echo "Status while SteamVR runs: cat /dev/shm/openrigcue/steamvr-driver.status"
echo "Uninstall: $here/uninstall.sh"
