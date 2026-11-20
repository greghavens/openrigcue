#!/bin/sh
# Cross-builds the plugin and links it under its
# file name with its five exports:
#   dist/rFactor2SharedMemoryMapPlugin64.dll  (x86_64-pc-windows-gnu)
# plus the Wine test programs dist/rf2-host.exe and dist/StockCarRules.dll
# (fake-scr). The crate is built as a static library and linked here with the
# .def file because a cargo cdylib also exports every #[no_mangle] symbol
# (DllMain, rust_eh_personality), which the plugin must not.
# Run inside the toolbox:  toolbox run -c openrigcue-audio sh scripts/build.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
: "${CARGO_TARGET_DIR:=$here/../../target/bin/rfactor2}"
export CARGO_TARGET_DIR
cargo=${CARGO:-$HOME/.cargo/bin/cargo}
cc=${MINGW64_GCC:-x86_64-w64-mingw32-gcc}
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$cc"
t=x86_64-pc-windows-gnu
cd "$here"
"$cargo" build --release --target $t -p rfactor2-smmp
"$cargo" build --release --target $t -p rf2-host -p fake-scr
out="$CARGO_TARGET_DIR/$t/release"
mkdir -p dist
# DllMainCRTStartup (mingw dllcrt2.o) -> our DllMain; msvcrt for malloc/free/atan2.
"$cc" -shared -o dist/rFactor2SharedMemoryMapPlugin64.dll \
    rFactor2SharedMemoryMapPlugin64.def "$out/librfactor2_smmp.a" \
    -Wl,--gc-sections -Wl,--dynamicbase -Wl,--nxcompat -Wl,--high-entropy-va -s \
    -lkernel32 -ladvapi32 -lmsvcrt
cp "$out/rf2-host.exe" dist/rf2-host.exe
cp "$out/StockCarRules.dll" dist/StockCarRules.dll
ls -l dist
