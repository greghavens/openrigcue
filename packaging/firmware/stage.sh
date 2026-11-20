#!/usr/bin/env bash
# Stage approved firmware inside the one AppImage; unapproved candidates stay outside it.
set -euo pipefail
[[ $# == 1 ]] || { echo "usage: $0 APPDIR" >&2; exit 2; }
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
manifest="$root/packaging/firmware/images.tsv"
dest=$1
declare -A seen=()
sources=() targets=()
while IFS=$'\t' read -r device source hash extra || [[ -n "$device" ]]; do
    [[ -n "$device" && "$device" != \#* ]] || continue
    [[ -z "$extra" && "$hash" =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid firmware manifest row: $device" >&2; exit 1; }
    [[ -z "${seen[$device]:-}" ]] || { echo "duplicate firmware device: $device" >&2; exit 1; }
    seen[$device]=1
    case $device in
        intellibox-v5) target=firmware/intellibox/intellibox.hex ;;
        intellibox-v6) target=firmware/intellibox/intellibox_v6.hex ;;
        belt) target=firmware/belt/firmware.bin ;;
        obd2hud) target=firmware/obd2hud/firmware.bin ;;
        *) echo "unknown firmware device: $device" >&2; exit 1 ;;
    esac
    [[ "$source" != /* && "$source" != *..* && "$source" != *$'\n'* && -f "$root/$source" ]] || {
        echo "invalid firmware artifact: $device" >&2; exit 1;
    }
    actual=$(sha256sum "$root/$source")
    actual=${actual%% *}
    [[ "$actual" == "$hash" ]] || { echo "firmware hash changed: $device" >&2; exit 1; }
    sources+=("$root/$source")
    targets+=("$dest/usr/bin/$target")
done < "$manifest"
for index in "${!sources[@]}"; do
    install -Dm0644 -- "${sources[$index]}" "${targets[$index]}"
done
install -Dm0644 "$manifest" "$dest/usr/share/openrigcue/firmware.tsv"
if [[ -n "${seen[belt]:-}${seen[obd2hud]:-}" ]]; then
    notice_root="$dest/usr/share/licenses/openrigcue/esp32-firmware"
    install -Dm0644 "$root/packaging/firmware/ESP32-RUNTIME-NOTICE.txt" "$notice_root/NOTICE.txt"
    for notice in "$root"/firmware/esp-idf33-sys/licenses/*.txt; do
        install -Dm0644 "$notice" "$notice_root/$(basename "$notice")"
    done
fi
