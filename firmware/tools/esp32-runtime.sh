#!/usr/bin/env bash
# Pinned public Arduino1.0.6 / IDF3.3.5 substrate. No Python, firmware execution,
# installed toolchain changes, device access or image promotion.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
runtime=${OPENRIGCUE_IDF33_ROOT:-$repo/target/bin/.esp32-runtime-idf33}
mkdir -p "$runtime"
runtime=$(realpath "$runtime")
# Cargo invokes this from both target build and final link; serialize only the
# private shared runtime cache so independent firmware builds cannot race it.
exec 9>"$runtime/.prepare.lock"
flock 9
download() {
    local file=$1 url=$2 digest=$3
    if [[ ! -f "$runtime/$file" ]]; then
        curl -fL --retry 2 "$url" -o "$runtime/$file.download" >&2
        printf '%s  %s\n' "$digest" "$runtime/$file.download" | sha256sum --check --status
        mv "$runtime/$file.download" "$runtime/$file"
    fi
    printf '%s  %s\n' "$digest" "$runtime/$file" | sha256sum --check --status
}
if [[ ! -d "$runtime/esp32-1.0.6/tools/sdk" ]]; then
    download esp32-1.0.6.zip https://github.com/espressif/arduino-esp32/releases/download/1.0.6/esp32-1.0.6.zip 982da9aaa181b6cb9c692dd4c9622b022ecc0d1e3aa0c5b70428ccc3c1b4556b
    unzip -q "$runtime/esp32-1.0.6.zip" -d "$runtime"
fi
if [[ ! -x "$runtime/xtensa-esp32-elf/bin/xtensa-esp32-elf-gcc" ]]; then
    download xtensa-gcc5.tar.gz https://github.com/espressif/arduino-esp32/releases/download/1.0.5-rc5/xtensa-esp32-elf-linux64-1.22.0-97-gc752ad5-5.2.0.tar.gz 96f5f6e7611a0ed1dc47048c54c3113fc5cebffbf0ba90d8bfcd497afc7ef9f3
    tar -xzf "$runtime/xtensa-gcc5.tar.gz" -C "$runtime"
fi
if [[ ! -d "$runtime/NimBLE-Arduino/.git" ]]; then
    git clone --quiet --filter=blob:none --no-checkout https://github.com/h2zero/NimBLE-Arduino.git "$runtime/NimBLE-Arduino"
    git -C "$runtime/NimBLE-Arduino" checkout --quiet 725079ade938d5e0f4de16695ba2b9039f5ffeb8
fi
[[ $(git -C "$runtime/NimBLE-Arduino" rev-parse HEAD) == 725079ade938d5e0f4de16695ba2b9039f5ffeb8 ]]
printf '%s\n' "$runtime"
