#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
runtime=$(bash "$root/firmware/tools/esp32-runtime.sh")
out=${1:?Usage: check-abi.sh PRIVATE_OUTPUT OBD_TARGET_DIR}
target=${2:?Usage: check-abi.sh PRIVATE_OUTPUT OBD_TARGET_DIR}
mkdir -p "$out"
out=$(realpath "$out")
mapfile -t bindings < <(find "$target/xtensa-esp32-espidf/release/build" -path '*/openrigcue-esp-idf33-sys-*/out/bindings.rs')
[[ ${#bindings[@]} == 1 ]]
mapfile -t cores < <(find "$target/xtensa-esp32-espidf/release/deps" -name 'libcore-*.rlib')
[[ ${#cores[@]} == 1 ]]
mapfile -t builtins < <(find "$target/xtensa-esp32-espidf/release/deps" -name 'libcompiler_builtins-*.rlib')
[[ ${#builtins[@]} == 1 ]]
includes=();for dir in "$runtime/esp32-1.0.6/tools/sdk/include/"*;do [[ ! -d "$dir" ]]||includes+=("-I$dir");done
includes+=("-I$runtime/NimBLE-Arduino/src")
while IFS= read -r dir;do includes+=("-I$dir");done < <(find "$runtime/NimBLE-Arduino/src" -type d -name include)
"$runtime/xtensa-esp32-elf/bin/xtensa-esp32-elf-gcc" -c -Os -DOPENRIGCUE_NIMBLE -DESP_PLATFORM -DARDUINO_ARCH_ESP32 -DARDUINO=10607 -DHAVE_CONFIG_H -DWITH_POSIX -DGCC_NOT_5_2_0=0 "${includes[@]}" "$root/firmware/tools/esp32-final/abi.c" -o "$out/c.o"
ABI_BINDINGS="${bindings[0]}" "${RUSTC:-$HOME/.cargo/bin/rustc}" +esp --edition=2021 --crate-type lib --target xtensa-esp32-espidf --extern core="${cores[0]}" --extern compiler_builtins="${builtins[0]}" -L dependency="$target/xtensa-esp32-espidf/release/deps" -O --emit=obj "$root/firmware/tools/esp32-final/abi.rs" -o "$out/rust.o"
for lang in c rust;do "$runtime/xtensa-esp32-elf/bin/xtensa-esp32-elf-objcopy" -O binary -j .rodata.orc_abi "$out/$lang.o" "$out/$lang.bin";done
cmp "$out/c.bin" "$out/rust.bin"
node - "$out/c.bin" <<'JS'
const fs=require('fs'),crypto=require('crypto');let b=fs.readFileSync(process.argv[2]);console.log(JSON.stringify({abiWords:b.length/4,sha256:crypto.createHash('sha256').update(b).digest('hex'),words:Array.from({length:b.length/4},(_,i)=>b.readUInt32LE(i*4))}));
JS
