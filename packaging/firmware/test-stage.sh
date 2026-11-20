#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/repo/packaging/firmware" "$work/repo/target/verified"
cp "$root/packaging/firmware/stage.sh" "$work/repo/packaging/firmware/stage.sh"
stage="$work/repo/packaging/firmware/stage.sh"
manifest="$work/repo/packaging/firmware/images.tsv"
printf '# empty verified set\n' > "$manifest"
bash "$stage" "$work/empty"
test -f "$work/empty/usr/share/openrigcue/firmware.tsv"
test ! -e "$work/empty/usr/bin/firmware"
printf ':00000001FF\n' > "$work/repo/target/verified/controller.hex"
hash=$(sha256sum "$work/repo/target/verified/controller.hex")
hash=${hash%% *}
printf 'intellibox-v5\ttarget/verified/controller.hex\t%s\n' "$hash" > "$manifest"
bash "$stage" "$work/good"
cmp "$work/repo/target/verified/controller.hex" "$work/good/usr/bin/firmware/intellibox/intellibox.hex"
printf 'changed\n' >> "$work/repo/target/verified/controller.hex"
if bash "$stage" "$work/bad-hash" > /dev/null 2>&1; then exit 1; fi
test ! -e "$work/bad-hash/usr/bin/firmware"
printf 'invalid\ttarget/verified/controller.hex\t%s\n' "$hash" > "$manifest"
if bash "$stage" "$work/bad-id" > /dev/null 2>&1; then exit 1; fi
printf 'intellibox-v5\t../controller.hex\t%s\n' "$hash" > "$manifest"
if bash "$stage" "$work/bad-path" > /dev/null 2>&1; then exit 1; fi
printf ':00000001FF\n' > "$work/repo/target/verified/controller.hex"
printf 'intellibox-v5\ttarget/verified/controller.hex\t%s\n' "$hash" > "$manifest"
cat "$manifest" > "$work/row"
cat "$work/row" >> "$manifest"
if bash "$stage" "$work/duplicate" > /dev/null 2>&1; then exit 1; fi
test ! -e "$work/duplicate/usr/bin/firmware"
printf 'firmware staging: empty, verified, changed hash, unknown device, traversal and duplicate cases passed\n'
