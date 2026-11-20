#!/usr/bin/env bash
# Execute only a newly built Rust candidate on a private emulated ESP32.
# A missing peripheral is retained as a failed/unavailable end-to-end check.
set -euo pipefail
qemu=${1:?Usage: check-boot.sh QEMU NEW_RUST_MERGED_IMAGE NEW_PRIVATE_OUTPUT ELF}
merged=${2:?}
out=${3:?}
elf=${4:?}
mkdir "$out"
out=$(realpath "$out")
node - "$merged" "$out/flash.bin" <<'JS'
const fs=require('fs');let b=Buffer.alloc(4*1024*1024,255),a=fs.readFileSync(process.argv[2]);if(a.length>b.length)throw Error('Flash image oversized');a.copy(b);fs.writeFileSync(process.argv[3],b);
JS
sha256sum "$merged" "$elf" > "$out/inputs.sha256"
set +e
timeout --kill-after=2s 12s "$qemu" -nographic -machine esp32 -drive "file=$out/flash.bin,if=mtd,format=raw" -nic none > "$out/serial.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$out/qemu-exit.txt"
# No success is inferred from a timeout or a bootloader banner.
if [[ $status != 0 ]];then
    echo "Candidate boot did not complete; inspect $out/serial.log" >&2
    exit "$status"
fi
