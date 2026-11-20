#!/usr/bin/env bash
# Prepare the FMOD 2.02 Linux runtime libraries used by the AppImage from the official FMOD Engine
# download. Nothing is fetched from the network: FMOD's downloads need an fmod.com account, so the
# archive is supplied by whoever builds.
#
#   1. Sign in at https://www.fmod.com/download and download "FMOD Engine" 2.02.35 for Linux
#      (fmodstudioapi20235linux.tar.gz).
#   2. Put it at target/native-deps/fmodstudioapi20235linux.tar.gz, or set FMOD_ARCHIVE to its path.
#
# Only the two release runtime libraries are taken; no SDK headers, examples or media are installed.
set -euo pipefail
[[ $# == 1 ]] || { echo "usage: $0 <runtime-cache-directory>" >&2; exit 2; }
cache=$1
archive_name=fmodstudioapi20235linux.tar.gz
archive_root=fmodstudioapi20235linux
core="$cache/api/core/lib/x86_64/libfmod.so.13.35"
studio="$cache/api/studio/lib/x86_64/libfmodstudio.so.13.35"
core_hash=da68519e9d9eafc291a6db288d708794055776c47bcb4e756bb5eb3ada441e74
studio_hash=3f0e776883fb46e8d0e62cba6f6f3bba5862c2e4ffcd1bfc9af72b3f30b032d7
valid_runtime() {
    [[ -f "$core" && -f "$studio" && -f "$cache/FMOD-RUNTIME-SOURCE.txt" ]] || return 1
    grep -q "^Source: official FMOD Engine download" "$cache/FMOD-RUNTIME-SOURCE.txt" || return 1
    printf '%s  %s\n%s  %s\n' "$core_hash" "$core" "$studio_hash" "$studio" | sha256sum --check --status
}
mkdir -p "$cache"
if ! valid_runtime; then
    archive=${FMOD_ARCHIVE:-$(dirname "$cache")/$archive_name}
    if [[ ! -f "$archive" ]]; then
        cat >&2 <<TXT
FMOD Engine archive not found: $archive
Sign in at https://www.fmod.com/download, download "FMOD Engine" 2.02.35 for Linux ($archive_name)
and put it there, or set FMOD_ARCHIVE to its path. FMOD_SDK may instead point to an unpacked 2.02 SDK.
TXT
        exit 1
    fi
    work=$(mktemp -d "$cache/.unpack.XXXXXX")
    trap 'rm -rf -- "$work"' EXIT
    tar -xzf "$archive" -C "$work" \
        "$archive_root/api/core/lib/x86_64/libfmod.so.13.35" \
        "$archive_root/api/studio/lib/x86_64/libfmodstudio.so.13.35"
    printf '%s  %s\n%s  %s\n' \
        "$core_hash" "$work/$archive_root/api/core/lib/x86_64/libfmod.so.13.35" \
        "$studio_hash" "$work/$archive_root/api/studio/lib/x86_64/libfmodstudio.so.13.35" | sha256sum --check --status
    install -Dm0644 "$work/$archive_root/api/core/lib/x86_64/libfmod.so.13.35" "$core"
    install -Dm0644 "$work/$archive_root/api/studio/lib/x86_64/libfmodstudio.so.13.35" "$studio"
    cat > "$cache/FMOD-RUNTIME-SOURCE.txt" <<'TXT'
Made with FMOD Studio by Firelight Technologies Pty Ltd.

FMOD Engine 2.02.35 Linux x86_64 runtime libraries, by Firelight Technologies.
Source: official FMOD Engine download (fmodstudioapi20235linux.tar.gz) from https://www.fmod.com/download
License: https://www.fmod.com/legal

Only the two runtime libraries are included; no SDK headers, documentation, examples or media are
redistributed.

FMOD remains proprietary software governed by Firelight Technologies' terms.
OpenRigCue's GPL linking exception does not replace or expand those terms.
TXT
fi
valid_runtime
echo "FMOD 2.02.35 runtime verified: $cache"
