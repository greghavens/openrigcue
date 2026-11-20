#!/usr/bin/env bash
# Preserve the existing toolbox command while using the installed host Rust/
# Node tools. Only task-local environment is forwarded; no global settings.
cd "$here"
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR=$(realpath "$CARGO_TARGET_DIR")
export CARGO_TARGET_DIR
if [[ -f /run/.toolboxenv ]]; then
    args=(--host "--env=CARGO_TARGET_DIR=$CARGO_TARGET_DIR")
    for setting in OPENRIGCUE_IDF33_ROOT LIBCLANG_PATH CARGO ESPFLASH RUSTFLAGS CARGO_ENCODED_RUSTFLAGS; do
        if [[ -n ${!setting:-} ]]; then args+=("--env=$setting=${!setting}"); fi
    done
    exec flatpak-spawn "${args[@]}" bash "$here/make-image.sh"
fi
if [[ -z ${LIBCLANG_PATH:-} ]]; then
    candidates=("$HOME"/.rustup/toolchains/esp/xtensa-esp32-elf-clang/*/esp-clang/lib/libclang.so)
    if [[ ${#candidates[@]} == 1 && -f ${candidates[0]} ]]; then
        LIBCLANG_PATH=$(dirname "${candidates[0]}")
        export LIBCLANG_PATH
    fi
fi
: "${ESPFLASH:=$HOME/.cargo/bin/espflash}"
