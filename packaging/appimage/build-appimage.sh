#!/bin/bash
# Builds target/appimage/openrigcue-x86_64.AppImage. CARGO_TARGET_DIR selects compiled inputs only. Run inside the build
# container (packaging/build.Containerfile) from the repository root.
set -euo pipefail

stage_only=0
build=1
for arg in "$@"; do
    case "$arg" in
        --stage-only) stage_only=1 ;;
        --no-build) build=0 ;;
        *) echo "usage: $0 [--stage-only] [--no-build]" >&2; exit 2 ;;
    esac
done
root=$(pwd)
target="${CARGO_TARGET_DIR:-$root/target}"
out="$root/target/appimage"
cache="$root/target/.appimage-build"
appdir="$cache/AppDir"
mkdir -p "$cache" "$out"

# The one full build: every part (shaker audio, FMOD engine sound, OBD2HUD, ...) is always compiled in.
if ((build)); then
    cargo build --release --locked -p openrigcue -p openrigcue-steamvr-driver -p openrigcue-xr-layer
    cargo build --release --locked -p openrigcue-bridge --target x86_64-pc-windows-gnu
    CARGO_TARGET_DIR="${OPENRIGCUE_TRACKER_TARGET_DIR:-$target/bin/steamvr}" \
        cargo build --release --locked --manifest-path steamvr-driver/Cargo.toml
fi

rm -rf "$appdir"
install -Dm0755 "$target/release/openrigcue" "$appdir/usr/bin/openrigcue"
install -Dm0755 "$target/x86_64-pc-windows-gnu/release/openrigcue-bridge.exe" "$appdir/usr/lib/openrigcue/openrigcue-bridge.exe"
install -Dm0755 "$target/x86_64-pc-windows-gnu/release/openrigcue-automation.exe" "$appdir/usr/lib/openrigcue/openrigcue-automation.exe"
install -Dm0755 crates/openrigcue-bridge/scripts/openrigcue-bridge-run "$appdir/usr/lib/openrigcue/openrigcue-bridge-run"
install -Dm0644 packaging/udev/70-openrigcue.rules "$appdir/usr/share/openrigcue/70-openrigcue.rules"
install -Dm0755 packaging/udev/install-device-access "$appdir/usr/lib/openrigcue/install-device-access"
install -Dm0644 LICENSES/CPython-3.13.txt "$appdir/usr/share/licenses/openrigcue/CPython-3.13.txt"
install -Dm0644 LICENSES/Unicode.txt "$appdir/usr/share/licenses/openrigcue/Unicode.txt"
install -Dm0644 LICENSES/Lewton-0.10.2.txt "$appdir/usr/share/licenses/openrigcue/Lewton-0.10.2.txt"
install -Dm0644 LICENSES/Ogg-BSD.txt "$appdir/usr/share/licenses/openrigcue/Ogg-BSD.txt"
install -Dm0644 LICENSES/Xiph-Vorbis.txt "$appdir/usr/share/licenses/openrigcue/Xiph-Vorbis.txt"
install -Dm0644 LICENSES/libsqlite3-sys-0.30.1.txt "$appdir/usr/share/licenses/openrigcue/libsqlite3-sys-0.30.1.txt"
install -Dm0644 LICENSES/CPython-3.13-NOTICE.txt "$appdir/usr/share/licenses/openrigcue/CPython-3.13-NOTICE.txt"
install -Dm0644 LICENSES/sqlitedict-2.1.0.txt "$appdir/usr/share/licenses/openrigcue/sqlitedict-2.1.0.txt"
install -Dm0644 LICENSES/sqlitedict-2.1.0-NOTICE.txt "$appdir/usr/share/licenses/openrigcue/sqlitedict-2.1.0-NOTICE.txt"
install -Dm0644 LICENSES/Apache-2.0.txt "$appdir/usr/share/licenses/openrigcue/Apache-2.0.txt"
install -Dm0644 LICENSES/remi-NOTICE.txt "$appdir/usr/share/licenses/openrigcue/remi-NOTICE.txt"
install -Dm0644 COPYING "$appdir/usr/share/licenses/openrigcue/COPYING"
install -Dm0644 LICENSE-EXCEPTION.md "$appdir/usr/share/licenses/openrigcue/LICENSE-EXCEPTION.md"
install -Dm0644 LICENSES/IMask.txt "$appdir/usr/share/licenses/openrigcue/IMask.txt"
install -Dm0644 LICENSES/core-js-3.21.0.txt "$appdir/usr/share/licenses/openrigcue/core-js-3.21.0.txt"
install -Dm0644 LICENSES/DMSans-OFL-1.1.txt "$appdir/usr/share/licenses/openrigcue/DMSans-OFL-1.1.txt"
install -Dm0644 LICENSES/BarlowCondensed-OFL-1.1.txt "$appdir/usr/share/licenses/openrigcue/BarlowCondensed-OFL-1.1.txt"
for notice in num-bigint-0.4.8-MIT num-integer-0.1.47-MIT num-traits-0.2.19-MIT; do
    install -Dm0644 "LICENSES/$notice.txt" "$appdir/usr/share/licenses/openrigcue/$notice.txt"
done
install -Dm0644 packaging/systemd/openrigcue.service "$appdir/usr/share/openrigcue/openrigcue.service"
install -Dm0644 packaging/appimage/openrigcue.desktop "$appdir/openrigcue.desktop"
install -Dm0644 packaging/appimage/openrigcue.desktop "$appdir/usr/share/applications/openrigcue.desktop"
install -Dm0644 packaging/appimage/openrigcue.svg "$appdir/openrigcue.svg"
install -Dm0644 packaging/appimage/openrigcue.svg "$appdir/usr/share/icons/hicolor/scalable/apps/openrigcue.svg"
install -Dm0755 packaging/appimage/AppRun "$appdir/AppRun"
install -Dm0644 packaging/appimage/graphics-env.sh "$appdir/usr/lib/openrigcue/graphics-env.sh"

packaging/appimage/stage-avrdude.sh "$appdir"
bash packaging/firmware/stage.sh "$appdir"
packaging/vr/stage.sh "$appdir/usr/share/openrigcue/vr"

# Bundle ALSA (PipeWire and PulseAudio provide an ALSA endpoint). The native
# desktop uses host GTK 3/WebKitGTK 4.1; see README for runtime dependencies.
libasound=$(ldconfig -p | awk '/libasound\.so\.2 / && /x86-64/ {print $NF; exit}')
install -Dm0644 "$libasound" "$appdir/usr/lib/libasound.so.2"

# hookAxis uses SDL2's native joystick API. Some distributions provide this
# SONAME through sdl2-compat, which loads SDL3 dynamically rather than via DT_NEEDED.
libsdl2=$(ldconfig -p | awk '/libSDL2-2.0\.so\.0 / && /x86-64/ {print $NF; exit}')
install -Dm0644 "$libsdl2" "$appdir/usr/lib/libSDL2-2.0.so.0"
install -Dm0644 LICENSES/SDL2.txt "$appdir/usr/share/licenses/openrigcue/SDL2.txt"
if LC_ALL=C grep -a -q -F 'libSDL3.so.0' "$libsdl2"; then
    libsdl3=$(ldconfig -p | awk '/libSDL3\.so\.0 / && /x86-64/ {print $NF; exit}')
    install -Dm0644 "$libsdl3" "$appdir/usr/lib/libSDL3.so.0"
    install -Dm0644 LICENSES/SDL2-compat.txt "$appdir/usr/share/licenses/openrigcue/SDL2-compat.txt"
    install -Dm0644 LICENSES/SDL3.txt "$appdir/usr/share/licenses/openrigcue/SDL3.txt"
fi

# The full AppImage includes the FMOD runtime for imported engine banks.
# FMOD_SDK may point to another compatible 2.02 SDK; otherwise use the pinned cache.
fmod_source=${FMOD_SDK:-$root/target/native-deps/fmod}
if [[ -z "${FMOD_SDK:-}" ]]; then
    bash packaging/fmod/prepare-runtime.sh "$fmod_source"
fi
bash packaging/fmod/add-fmod.sh "$fmod_source" "$appdir"

# Check every ELF, including nested VR libraries and optional FMOD. The intended
# container baseline is 2.35; a newer host build must declare its real floor.
mkdir -p "$appdir/usr/share/openrigcue"
perl packaging/appimage/check-glibc.pl "$appdir" "${OPENRIGCUE_GLIBC_MAX:-2.35}" \
    "$appdir/usr/share/openrigcue/glibc-baseline.json"

desktop-file-validate "$appdir/openrigcue.desktop"
if ((stage_only)); then
    echo "staged $appdir"
    exit 0
fi
runtime_args=()
if [[ -n "${APPIMAGE_RUNTIME_FILE:-}" ]]; then
    runtime_args+=(--runtime-file "$APPIMAGE_RUNTIME_FILE")
fi
# Keep a running/mounted image's inode intact until the replacement is complete.
candidate_dir=$(mktemp -d "$cache/package.XXXXXX")
trap 'rm -rf -- "$candidate_dir"' EXIT
candidate="$candidate_dir/image"
ARCH=x86_64 appimagetool --no-appstream --comp xz "${runtime_args[@]}" "$appdir" "$candidate"
test -s "$candidate"
test -x "$candidate"
offset=$("$candidate" --appimage-offset)
[[ "$offset" =~ ^[0-9]+$ ]]
unsquashfs -cat -offset "$offset" "$candidate" usr/bin/openrigcue | cmp - "$appdir/usr/bin/openrigcue"
mv -f -- "$candidate" "$out/openrigcue-x86_64.AppImage"
echo "built $out/openrigcue-x86_64.AppImage"
