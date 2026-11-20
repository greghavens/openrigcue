#!/bin/bash
# Pure shell policy/argument tests. Never starts OpenRigCue, GTK or a device worker.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fixture=$(mktemp -d /tmp/openrigcue-graphics-fixture.XXXXXX)
trap 'rm -rf -- "$fixture"' EXIT
printf '610.57.04\n' > "$fixture/nvidia-version"
helper=$root/packaging/appimage/graphics-env.sh
checks=0
check_policy() {
    local name=$1 expected=$2
    shift 2
    local actual
    # The quoted program expands its own positional parameters in the child shell.
    # shellcheck disable=SC2016
    actual=$(env -i PATH="$PATH" "$@" bash --noprofile --norc -c '
        source "$1"
        module_version=$2
        shift 2
        openrigcue_apply_graphics_compatibility "$module_version" "$@"
        if [[ -v __NV_DISABLE_EXPLICIT_SYNC ]]; then printf "set:%s" "$__NV_DISABLE_EXPLICIT_SYNC"; else printf unset; fi
    ' bash "$helper" "$fixture/nvidia-version" run)
    [[ $actual == "$expected" ]] || { printf '%s: expected [%s], got [%s]\n' "$name" "$expected" "$actual" >&2; exit 1; }
    checks=$((checks+1))
}
check_policy wayland_nvidia set:1 WAYLAND_DISPLAY=wayland-0
check_policy explicit_zero set:0 WAYLAND_DISPLAY=wayland-0 __NV_DISABLE_EXPLICIT_SYNC=0
check_policy explicit_empty set: WAYLAND_DISPLAY=wayland-0 __NV_DISABLE_EXPLICIT_SYNC=
check_policy explicit_one set:1 WAYLAND_DISPLAY=wayland-0 __NV_DISABLE_EXPLICIT_SYNC=1
check_policy no_wayland unset
check_policy x11_only unset WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=x11
check_policy broadway_only unset WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=broadway
check_policy wayland_fallback set:1 WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=x11,wayland
check_policy wildcard_backend set:1 WAYLAND_DISPLAY=wayland-0 GDK_BACKEND='*'
# shellcheck disable=SC2016
actual=$(env -i PATH="$PATH" WAYLAND_DISPLAY=wayland-0 bash --noprofile --norc -c 'source "$1";openrigcue_apply_graphics_compatibility "$2" run;printf "%s" "${__NV_DISABLE_EXPLICIT_SYNC-unset}"' bash "$helper" "$fixture/missing-module")
[[ $actual == unset ]]
checks=$((checks+1))

# Exercise the real entry-point text with only its module-probe path replaced.
# The executed binary is a disposable argument/environment recorder, not the app.
mkdir -p "$fixture/app/usr/bin" "$fixture/app/usr/lib/openrigcue"
sed "s|/sys/module/nvidia/version|$fixture/nvidia-version|" "$root/packaging/appimage/AppRun" > "$fixture/app/AppRun"
cp "$helper" "$fixture/app/usr/lib/openrigcue/graphics-env.sh"
cat > "$fixture/app/usr/bin/openrigcue" <<'STUB'
#!/bin/bash
printf '%s\n' "${__NV_DISABLE_EXPLICIT_SYNC-unset}" > "$OPENRIGCUE_TEST_CAPTURE"
printf '<%s>\n' "$@" >> "$OPENRIGCUE_TEST_CAPTURE"
STUB
chmod +x "$fixture/app/usr/bin/openrigcue"
check_entry() {
    local name=$1 expected_env=$2
    shift 2
    env -i PATH="$PATH" WAYLAND_DISPLAY=wayland-0 OPENRIGCUE_TEST_CAPTURE="$fixture/capture" bash --noprofile --norc "$fixture/app/AppRun" "$@"
    {
        printf '%s\n' "$expected_env"
        if (($#)); then printf '<%s>\n' "$@"; else printf '<run>\n'; fi
    } > "$fixture/expected"
    cmp "$fixture/capture" "$fixture/expected" || { printf 'entry mismatch: %s\n' "$name" >&2; exit 1; }
    checks=$((checks+1))
}
check_entry default 1
check_entry explicit_run 1 run
check_entry global_log 1 --log debug run --config-dir '/tmp/config path'
check_entry joined_log 1 --log=debug run
check_entry native_tools 1 tools
check_entry tools_port 1 tools --port 33735
check_entry tools_log 1 --log debug tools
check_entry tools_headless unset tools --headless
check_entry tools_help unset tools --help
check_entry tools_report unset tools --usb-report '/tmp/report file.xml' --xml
check_entry headless unset run --headless
check_entry help unset --help
check_entry run_help unset run --help
check_entry version unset --version
check_entry other_command unset games --json
printf '%s pure graphics-policy/entry-point checks passed; no app/GTK/hardware executed\n' "$checks"
