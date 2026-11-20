#!/bin/sh
# Cross-builds the plugin and links it with
# its four exports and internal name:
#   dist/SRS_Rfactor1.dll  (i686-pc-windows-gnu)
# plus the Wine test programs dist/rf1-host.exe and dist/rf1-libm.exe
# (harness/). The crate is built
# as a static library and linked here with the .def file because a cargo
# cdylib would also export every #[no_mangle] symbol (DllMain,
# rust_eh_personality), which the plugin must not.
# Run inside the toolbox:  toolbox run -c openrigcue-audio sh scripts/build.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
: "${CARGO_TARGET_DIR:=$here/../../target/bin/rfactor1}"
export CARGO_TARGET_DIR
cargo=${CARGO:-$HOME/.cargo/bin/cargo}
cc=${MINGW32_GCC:-i686-w64-mingw32-gcc}
objcopy=${MINGW32_OBJCOPY:-i686-w64-mingw32-objcopy}
windres=${MINGW32_WINDRES:-i686-w64-mingw32-windres}
export CARGO_TARGET_I686_PC_WINDOWS_GNU_LINKER="$cc"
t=i686-pc-windows-gnu
cd "$here"
"$cargo" build --release --target $t -p rfactor1-smm
"$cargo" build --release --target $t -p rf1-host
out="$CARGO_TARGET_DIR/$t/release"
mkdir -p dist
# The plugin needs no unwind tables: drop the .eh_frame of the
# Rust objects (panic = abort, nothing reads them) so the image has no
# .eh_fram section.
"$objcopy" -R .eh_frame "$out/librfactor1_smm.a" "$out/rf1-noeh.a"
# .rsrc: RT_MANIFEST id 2, lang 0x409 (res/SRS_Rfactor1.manifest).
"$windres" -O coff -i res/SRS_Rfactor1.rc -o "$out/rf1-res.o"
# No C runtime: the plugin imports only KERNEL32, so the DLL
# is linked without mingw's dllcrt2.o / msvcrt; DllMain is the entry point
# and src/rt.rs carries the CRT pieces (memset, heap, abort/exit, RTTI).
# Optional header: ImageBase 0x10000000, Windows GUI
# subsystem 6.0, OS version 6.0, image version 0.0, stack and heap
# 0x100000 reserve / 0x1000 commit, DYNAMIC_BASE | NX_COMPAT.
"$cc" -shared -nostdlib -nostartfiles -Wl,-e,_DllMain@12 \
    -o dist/SRS_Rfactor1.dll SRS_Rfactor1.def "$out/rf1-noeh.a" "$out/rf1-res.o" \
    -Wl,--gc-sections -Wl,--dynamicbase -Wl,--nxcompat -s -lkernel32 \
    -Wl,-u,__load_config_used -Wl,-Map,"$out/rf1.map" \
    -Wl,--image-base,0x10000000 -Wl,--subsystem,windows:6.0 \
    -Wl,--major-os-version,6 -Wl,--minor-os-version,0 \
    -Wl,--major-image-version,0 -Wl,--minor-image-version,0 \
    -Xlinker --stack -Xlinker 0x100000,0x1000 -Xlinker --heap -Xlinker 0x100000,0x1000 \
    -Wl,--no-insert-timestamp

# Header fields GNU ld (2.43) has no option for, written in place:
#   COFF TimeDateStamp 0x5ac84ccb, Characteristics 0x2102 (EXECUTABLE_IMAGE |
#   32BIT_MACHINE | DLL; ld adds the *_STRIPPED bits), MajorLinkerVersion 14
#   / Minor 0, CheckSum 0, the export directory's TimeDateStamp 0x5ac84cca,
#   and DataDirectory[10] (Load Configuration) = __load_config_used, size
#   0x40 (ld 2.43 does not fill it in).
dll=dist/SRS_Rfactor1.dll
put() { # put FILE OFFSET VALUE NBYTES  (little endian)
    v=$3; i=0; s=
    while [ $i -lt $4 ]; do
        s="$s$(printf '\\%03o' $((v & 255)))"; v=$((v >> 8)); i=$((i + 1))
    done
    printf "$s" | dd of="$1" bs=1 seek=$(($2)) conv=notrunc status=none
}
rd32() { od -An -tu4 -j $(($2)) -N4 "$1" | tr -d ' '; }
pe=$(rd32 $dll 0x3c)
opt=$((pe + 24))
put $dll $((pe + 8)) 0x5ac84ccb 4
put $dll $((pe + 22)) 0x2102 2
put $dll $((opt + 2)) 14 1
put $dll $((opt + 3)) 0 1
put $dll $((opt + 64)) 0 4
lc=$(awk '$2 == "_load_config_used" { print $1 }' "$out/rf1.map")
[ -n "$lc" ] || { echo "__load_config_used missing from the map" >&2; exit 1; }
put $dll $((opt + 96 + 10 * 8)) $((lc - 0x10000000)) 4
put $dll $((opt + 96 + 10 * 8 + 4)) 0x40 4
# export directory: RVA -> file offset through its section
erva=$(rd32 $dll $((opt + 96)))
eoff=$(objdump -h $dll | awk -v r=$erva '$1 ~ /^[0-9]+$/ { va = strtonum("0x" $4) - 0x10000000; sz = strtonum("0x" $3); if (r >= va && r < va + sz) print r - va + strtonum("0x" $6) }')
put $dll $((eoff + 4)) 0x5ac84cca 4
cp "$out/rf1-host.exe" dist/rf1-host.exe
cp "$out/rf1-libm.exe" dist/rf1-libm.exe
ls -l dist
