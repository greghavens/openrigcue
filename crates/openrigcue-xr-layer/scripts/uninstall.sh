#!/usr/bin/env bash
# Removes what install.sh installed for the current user.
#
#   uninstall.sh [--dry-run]
set -euo pipefail

dry_run=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        -h|--help) sed -n '2,4p' "$0"; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

data="${XDG_DATA_HOME:-$HOME/.local/share}"
for f in "$data/openxr/1/api_layers/implicit.d/openrigcue_xr_layer.json" "$data/openrigcue/xr-layer/libopenrigcue_xr_layer.so"; do
    if [[ -e "$f" ]]; then
        if [[ "$dry_run" == 1 ]]; then echo "would remove $f"; else rm -f "$f"; echo "removed $f"; fi
    fi
done
if [[ "$dry_run" == 0 ]]; then
    rmdir "$data/openrigcue/xr-layer" 2>/dev/null || true
fi
