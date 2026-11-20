#!/usr/bin/env bash
# Unregisters the OpenRigCue SteamVR driver (vrpathreg removedriver) and removes the staged folder.
# Restart SteamVR afterwards.
#
#   scripts/uninstall.sh [--dry-run] [--purge] [--prefix DIR]
#
#   --purge   also delete ~/.config/openrigcue/steamvr-driver.toml
set -euo pipefail

dry_run=0
purge=0
prefix="${XDG_DATA_HOME:-$HOME/.local/share}/openrigcue/steamvr-driver"
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1 ;;
        --purge) purge=1 ;;
        --prefix) prefix="$2"; shift ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

run() {
    echo "+ $*"
    if [ "$dry_run" = 0 ]; then "$@"; fi
}

steamvr="${STEAMVR:-}"
if [ -z "$steamvr" ]; then
    for d in "$HOME/.local/share/Steam/steamapps/common/SteamVR" \
             "$HOME/.steam/steam/steamapps/common/SteamVR" \
             "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common/SteamVR"; do
        if [ -x "$d/bin/vrpathreg.sh" ]; then steamvr="$d"; break; fi
    done
fi

dest="$prefix/openrigcue"
if [ -n "$steamvr" ] && [ -x "$steamvr/bin/vrpathreg.sh" ]; then
    run "$steamvr/bin/vrpathreg.sh" removedriver "$dest"
else
    echo "SteamVR not found: remove \"$dest\" from external_drivers in ~/.config/openvr/openvrpaths.vrpath yourself" >&2
fi
run rm -rf "$dest"
if [ "$purge" = 1 ]; then
    run rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/openrigcue/steamvr-driver.toml"
fi
if [ "$dry_run" = 1 ]; then echo "(dry run: nothing was changed)"; fi
echo "Removed. Restart SteamVR."
