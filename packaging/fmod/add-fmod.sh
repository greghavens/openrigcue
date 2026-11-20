#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Copies FMOD Engine 2.02 libraries from an SDK or prepared runtime cache into
# the OpenRigCue AppDir and includes attribution and available license notices.
#
#   packaging/fmod/add-fmod.sh <fmodstudioapi202xxlinux.tar.gz | extracted SDK dir> <AppDir>
#
# Result:
#   <AppDir>/usr/lib/openrigcue/fmod/libfmod.so.13*, libfmodstudio.so.13* (x86_64, release builds)
#   <AppDir>/usr/share/openrigcue/fmod/FMOD-ATTRIBUTION.txt (+ the SDK's licence text, if it has one)
#
# Nothing from the SDK is executed; files are only copied.
set -euo pipefail

usage() { echo "usage: $0 <FMOD Engine 2.02 Linux SDK (.tar.gz or directory)> <AppDir>" >&2; exit 2; }
[ $# -eq 2 ] || usage
sdk_in=$1
appdir=$2
[ -d "$appdir" ] || { echo "AppDir $appdir does not exist" >&2; exit 1; }

work=""
cleanup() {
    if [ -n "$work" ]; then
        rm -rf -- "$work"
    fi
}
trap cleanup EXIT

if [ -f "$sdk_in" ]; then
    # Unpack into a new, empty directory of its own.
    work=$(mktemp -d)
    tar -xzf "$sdk_in" -C "$work" --no-same-owner --no-same-permissions
    sdk=$work
elif [ -d "$sdk_in" ]; then
    sdk=$sdk_in
else
    echo "no such SDK: $sdk_in" >&2
    exit 1
fi

# api/core/lib/x86_64/libfmod.so.13.<patch> and api/studio/lib/x86_64/libfmodstudio.so.13.<patch>
# (the logging builds libfmodL / libfmodstudioL are left out).
find_lib() {
    find "$sdk" -path "*/api/$1/lib/x86_64/$2.so.13.*" -type f | sort -V | tail -n 1
}
core=$(find_lib core libfmod)
studio=$(find_lib studio libfmodstudio)
if [ -z "$core" ] || [ -z "$studio" ]; then
    echo "no FMOD 2.02 x86_64 libraries (api/core/lib/x86_64/libfmod.so.13.*, api/studio/lib/x86_64/libfmodstudio.so.13.*) in $sdk_in" >&2
    echo "OpenRigCue needs the FMOD Engine 2.02 Linux SDK (soname 13), as used by SRS (2.02.10)." >&2
    exit 1
fi
core_patch=${core##*.so.13.}
studio_patch=${studio##*.so.13.}
if [ "$core_patch" != "$studio_patch" ]; then
    echo "core ($core_patch) and studio ($studio_patch) libraries are from different FMOD releases" >&2
    exit 1
fi

dest="$appdir/usr/lib/openrigcue/fmod"
share="$appdir/usr/share/openrigcue/fmod"
rm -rf "$dest" "$share"
install -d "$dest" "$share"
for lib in "$core" "$studio"; do
    name=$(basename "$lib")            # libfmod.so.13.10
    base=${name%%.so.*}                # libfmod
    install -m0644 "$lib" "$dest/$name"
    ln -s "$name" "$dest/$base.so.13"
    ln -s "$base.so.13" "$dest/$base.so"
done

cat > "$share/FMOD-ATTRIBUTION.txt" <<TXT
Made with FMOD Studio by Firelight Technologies Pty Ltd.

This OpenRigCue AppImage contains the FMOD Engine 2.02.$core_patch libraries (libfmod, libfmodstudio)
by Firelight Technologies Pty Ltd, https://www.fmod.com. FMOD is not open-source software: it is
used under the FMOD End User Licence Agreement accepted by whoever built this AppImage, and is not
covered by OpenRigCue's GPL. OpenRigCue may be combined with it under the additional permission in
LICENSE-EXCEPTION.md of the OpenRigCue source.
TXT

# The SDK's own licence text, if the archive has one (its name has changed between releases).
licence=$(find "$sdk" -maxdepth 3 -type f \( -iname 'LICENSE*.TXT' -o -iname 'LICENCE*.TXT' -o -iname 'EULA*' \) | sort | head -n 1)
if [ -n "$licence" ]; then
    install -m0644 "$licence" "$share/$(basename "$licence")"
fi
if [[ -f "$sdk/FMOD-RUNTIME-SOURCE.txt" ]]; then
    install -m0644 "$sdk/FMOD-RUNTIME-SOURCE.txt" "$share/FMOD-RUNTIME-SOURCE.txt"
fi

echo "added FMOD 2.02.$core_patch to $dest"
