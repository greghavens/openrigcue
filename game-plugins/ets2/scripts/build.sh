#!/bin/sh
# Cross-builds the plugin (and the scs-host test program) for its
# two targets and copies the DLLs to their file names:
#   dist/ets2-telemetry.dll     (x86_64-pc-windows-gnu)
#   dist/ets2-telemetry_32.dll  (i686-pc-windows-gnu)
# Run inside the toolbox:  toolbox run -c openrigcue-audio sh scripts/build.sh
# Linkers: x86_64-w64-mingw32-gcc (Fedora mingw64-gcc); i686: $MINGW32_GCC or
# i686-w64-mingw32-gcc on PATH (Fedora mingw32-gcc).
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
: "${CARGO_TARGET_DIR:=$here/../../target/bin/ets2}"
export CARGO_TARGET_DIR
cargo=${CARGO:-$HOME/.cargo/bin/cargo}
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=${MINGW64_GCC:-x86_64-w64-mingw32-gcc}
export CARGO_TARGET_I686_PC_WINDOWS_GNU_LINKER=${MINGW32_GCC:-i686-w64-mingw32-gcc}
cd "$here"
for t in x86_64-pc-windows-gnu i686-pc-windows-gnu; do
    # Separate invocations: the harness enables the plugin's `std` feature,
    # the DLL itself is built without it.
    "$cargo" build --release --target "$t" -p scs-host
    "$cargo" build --release --target "$t" -p ets2-telemetry
done
mkdir -p dist
cp "$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/ets2_telemetry.dll" dist/ets2-telemetry.dll
cp "$CARGO_TARGET_DIR/i686-pc-windows-gnu/release/ets2_telemetry.dll" dist/ets2-telemetry_32.dll
cp "$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/scs-host.exe" dist/scs-host.exe
cp "$CARGO_TARGET_DIR/i686-pc-windows-gnu/release/scs-host.exe" dist/scs-host_32.exe
ls -l dist
