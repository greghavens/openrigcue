#!/usr/bin/env bash
# Link Rust objects with the pinned public IDF3.3.5 runtime, not IDF5.
set -euo pipefail
runtime=$(bash "$(dirname "$0")/esp32-runtime.sh")
sdk="$runtime/esp32-1.0.6/tools/sdk"
read -r -a libraries <<< "$(sed -n 's/^compiler.c.elf.libs=//p' "$runtime/esp32-1.0.6/platform.txt")"
# Resolve the SDK/libgcc arithmetic routines before Rust's generic
# fallback intrinsics.
arguments=();fallbacks=()
for argument in "$@"; do
    case "$argument" in
        -no-pie) ;; # GCC5 already links this embedded target without PIE.
        *libcompiler_builtins-*.rlib) fallbacks+=("$argument");;
        *) arguments+=("$argument");;
    esac
done
exec "$runtime/xtensa-esp32-elf/bin/xtensa-esp32-elf-gcc" "${arguments[@]}" \
    -nostdlib -L"$sdk/lib" -L"$sdk/ld" \
    -Tesp32_out.ld -Tesp32.project.ld -Tesp32.rom.ld -Tesp32.peripherals.ld \
    -Tesp32.rom.libgcc.ld -Tesp32.rom.spiram_incompatible_fns.ld \
    -u esp_app_desc -u ld_include_panic_highint_hdl -u call_user_start_cpu0 -u uxTopUsedPriority \
    -Wl,--gc-sections,-static,--start-group "${libraries[@]}" "${fallbacks[@]}" -Wl,--end-group
