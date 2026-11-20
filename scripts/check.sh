#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# CI-style check of the OpenRigCue workspace: formatting, build, tests and clippy of the one full build (every part
# always compiled in; the shaker audio needs the ALSA development headers). Run outside a toolbox, the script runs
# itself again inside the toolbox that has those headers.
#
# The tests run inside a bubblewrap sandbox when `bwrap` is available: read-only file system (so nothing is written
# to ~/.config, ~/.local or the source tree; the home directory stays readable, it is not hidden), a minimal /dev
# holding only null, zero, full, random, urandom, tty, ptmx and a private pts (no serial, input, sound or hidraw
# node), a new session without a controlling terminal (so opening /dev/tty fails with ENXIO), a private network
# namespace (no port of a running service or game can be taken), and empty writable stand-ins (fresh temporary
# directories) for $XDG_RUNTIME_DIR and /dev/shm (so no pipe-relay socket or bridge mirror of a running OpenRigCue
# is touched). A test that tries to write the file system, open a hardware or terminal node, or reach the host
# network fails. Writes below the runtime-dir or /dev/shm stand-ins succeed (a failed relay-socket bind is only
# logged by the code, so a read-only stand-in would hide it), and the step fails if either stand-in is not empty
# after the run, listing what was left there.
#
# usage: scripts/check.sh [--no-sandbox] [--no-fmt | --fix-fmt]
#   --no-sandbox  run the tests without the bubblewrap sandbox
#   --no-fmt      skip the formatting check
#   --fix-fmt     run `cargo fmt --all` instead of checking the formatting
# environment:
#   CARGO_TARGET_DIR          build directory (default: <repo>/target)
#   OPENRIGCUE_AUDIO_TOOLBOX  toolbox container with the ALSA headers (default: openrigcue-audio)
set -euo pipefail

sandbox=1
fix_fmt=0
fmt=1
for arg in "$@"; do
    case "$arg" in
        --no-sandbox) sandbox=0 ;;
        --no-fmt) fmt=0 ;;
        --fix-fmt) fix_fmt=1 ;;
        -h | --help)
            sed -n '3,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
toolbox_name="${OPENRIGCUE_AUDIO_TOOLBOX:-openrigcue-audio}"

# Use the host when its ALSA development metadata is available; otherwise enter the configured toolbox.
if [[ ! -e /run/.toolboxenv ]] && ! pkg-config --exists alsa; then
    if ! command -v toolbox >/dev/null; then
        echo "toolbox not found: the build needs the toolbox $toolbox_name (ALSA development headers)" >&2
        exit 1
    fi
    exec toolbox run -c "$toolbox_name" env PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
        bash "$root/scripts/check.sh" "$@"
fi

step() { printf '\n==> %s\n' "$*"; }

# Runs `cargo test <args>` in the bubblewrap sandbox described above; fails when a test left anything in the
# runtime-dir or /dev/shm stand-in.
sandboxed_test() {
    local shm runtime
    shm=$(mktemp -d)
    runtime=$(mktemp -d)
    local mounts=(
        --ro-bind / /
        --dev /dev
        --proc /proc
        --tmpfs /tmp
        --bind "$shm" /dev/shm
    )
    if [[ -n "${XDG_RUNTIME_DIR:-}" && -d "$XDG_RUNTIME_DIR" ]]; then
        mounts+=(--bind "$runtime" "$XDG_RUNTIME_DIR")
    fi
    mounts+=(--bind "$CARGO_TARGET_DIR" "$CARGO_TARGET_DIR")
    local status=0
    bwrap "${mounts[@]}" --unshare-net --unshare-pid --new-session --die-with-parent \
        --setenv TMPDIR /tmp --setenv CARGO_TARGET_DIR "$CARGO_TARGET_DIR" --chdir "$root" \
        cargo test --offline "$@" || status=$?
    local dir label leftovers
    for dir in "$runtime" "$shm"; do
        [[ "$dir" == "$runtime" ]] && label="\$XDG_RUNTIME_DIR" || label=/dev/shm
        leftovers=$(find "$dir" -mindepth 1 -printf '  %P\n')
        if [[ -n "$leftovers" ]]; then
            printf 'tests wrote below the %s stand-in:\n%s\n' "$label" "$leftovers" >&2
            status=1
        fi
        rm -rf -- "$dir"
    done
    return "$status"
}

if ((fix_fmt)); then
    step "cargo fmt --all"
    cargo fmt --all
elif ((fmt)); then
    step "cargo fmt --all --check"
    cargo fmt --all --check
fi

step "cargo build --workspace --all-targets"
cargo build --workspace --all-targets
cargo test --workspace --no-run --quiet

if ((sandbox)) && command -v bwrap >/dev/null; then
    step "cargo test --workspace (sandboxed: read-only fs, minimal /dev, no controlling tty, private network, runtime dir and /dev/shm stand-ins checked empty afterwards)"
    sandboxed_test --workspace --no-fail-fast
else
    ((sandbox)) && echo "bwrap not found: the tests run without the isolation sandbox" >&2
    step "cargo test --workspace"
    cargo test --workspace --no-fail-fast
fi

step "cargo clippy --workspace --all-targets"
cargo clippy --workspace --all-targets -- -D warnings

step "all checks passed"
