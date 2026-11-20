#!/usr/bin/env bash
# Installs the OpenRigCue OpenXR motion-compensation layer for the current user.
#
#   install.sh [--always-on] [--dry-run] [path/to/libopenrigcue_xr_layer.so]
#
# Copies the library to $XDG_DATA_HOME/openrigcue/xr-layer/ and writes the implicit-layer manifest
# to $XDG_DATA_HOME/openxr/1/api_layers/implicit.d/ (XDG_DATA_HOME defaults to ~/.local/share).
# Both live under $HOME, which Steam's pressure-vessel container shares with Proton games.
#
# By default the layer loads only in processes started with OPENRIGCUE_XR_LAYER_ENABLE=1 (for a
# Steam game: launch options "OPENRIGCUE_XR_LAYER_ENABLE=1 %command%"). --always-on drops that
# condition so the layer loads into every OpenXR application. Either way
# OPENRIGCUE_XR_LAYER_DISABLE=1 keeps it out of a process.
#
# Nothing outside $XDG_DATA_HOME is touched; no runtime or Steam configuration is changed.
set -euo pipefail

always_on=0
dry_run=0
lib=""
for arg in "$@"; do
    case "$arg" in
        --always-on) always_on=1 ;;
        --dry-run) dry_run=1 ;;
        -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
        -*) echo "unknown option: $arg" >&2; exit 2 ;;
        *) lib="$arg" ;;
    esac
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate="$(dirname "$here")"
if [[ -z "$lib" ]]; then
    for candidate in "${CARGO_TARGET_DIR:-$crate/../../target}/release/libopenrigcue_xr_layer.so"; do
        [[ -f "$candidate" ]] && lib="$candidate"
    done
fi
if [[ -z "$lib" || ! -f "$lib" ]]; then
    echo "libopenrigcue_xr_layer.so not found; build it with 'cargo build --release -p openrigcue-xr-layer' or pass its path" >&2
    exit 1
fi

data="${XDG_DATA_HOME:-$HOME/.local/share}"
lib_dir="$data/openrigcue/xr-layer"
manifest_dir="$data/openxr/1/api_layers/implicit.d"
lib_dest="$lib_dir/libopenrigcue_xr_layer.so"
manifest_dest="$manifest_dir/openrigcue_xr_layer.json"

# The path goes into a JSON string: refuse control characters, escape backslash and quote, and
# substitute with a quoted (literal) bash expansion so no character of the path is special.
if [[ "$lib_dest" == *[[:cntrl:]]* ]]; then
    echo "refusing a library path with control characters: $lib_dest" >&2
    exit 1
fi
json_path="${lib_dest//'\'/'\\'}"
json_path="${json_path//'"'/'\"'}"
template="$(<"$crate/manifest/openrigcue_xr_layer.json.in")"
manifest="${template//'@LIBRARY_PATH@'/"$json_path"}"
if [[ "$always_on" == 1 ]]; then
    manifest="$(printf '%s\n' "$manifest" | sed '/"enable_environment"/d')"
fi

run() {
    if [[ "$dry_run" == 1 ]]; then
        printf 'would run:'; printf ' %q' "$@"; printf '\n'
    else
        "$@"
    fi
}

run mkdir -p "$lib_dir" "$manifest_dir"
run install -m 0644 "$lib" "$lib_dest"
if [[ "$dry_run" == 1 ]]; then
    echo "would write $manifest_dest:"
    printf '%s\n' "$manifest"
else
    printf '%s\n' "$manifest" > "$manifest_dest.tmp"
    mv "$manifest_dest.tmp" "$manifest_dest"
    echo "installed $lib_dest"
    echo "installed $manifest_dest"
    if [[ "$always_on" == 1 ]]; then
        echo "the layer now loads into every OpenXR application (disable with OPENRIGCUE_XR_LAYER_DISABLE=1)"
    else
        echo "enable per game with OPENRIGCUE_XR_LAYER_ENABLE=1 (Steam launch options: OPENRIGCUE_XR_LAYER_ENABLE=1 %command%)"
    fi
fi
