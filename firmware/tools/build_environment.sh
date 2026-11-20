#!/bin/sh
# Shared environment for the two existing AVR firmware build entrypoints.
# Sourced only after the caller supplies root and FIRMWARE_BUILD_ENTRY.
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR=$(CDPATH='' cd -- "$CARGO_TARGET_DIR" && pwd)
export CARGO_TARGET_DIR

if [ -f /run/.toolboxenv ]; then
    # Toolbox has AVR libc/gcc but not the host LLVM22/Node installation. Its
    # normal user session already supplies the host bus used by flatpak-spawn.
    # Forward task-local overrides explicitly; never edit host configuration.
    set -- --host "--env=CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
    for setting in CARGO RUSTC LLVM_CONFIG CLANGXX LLVM_AR LLVM_OBJDUMP LLVM_OBJCOPY AVR_RUNTIME_ROOT OPENRIGCUE_AVR_TOOLBOX; do
        eval 'setting_value=${'"$setting"'-}'
        if [ -n "$setting_value" ]; then
            set -- "$@" "--env=$setting=$setting_value"
        fi
    done
    exec flatpak-spawn "$@" sh "$FIRMWARE_BUILD_ENTRY"
fi

# Preserve caller-selected tools; otherwise use the existing local LLVM22.
if [ -z "${LLVM_CONFIG:-}" ]; then
    if command -v llvm-config >/dev/null 2>&1; then
        LLVM_CONFIG=$(command -v llvm-config)
    elif [ -x /home/linuxbrew/.linuxbrew/bin/llvm-config ]; then
        LLVM_CONFIG=/home/linuxbrew/.linuxbrew/bin/llvm-config
    else
        echo 'The verified AVR build requires the existing host LLVM22 toolchain.' >&2
        exit 2
    fi
fi
LLVM_CONFIG=$(command -v "$LLVM_CONFIG")
llvm_bin=$(CDPATH='' cd -- "$(dirname -- "$LLVM_CONFIG")" && pwd)
PATH="$llvm_bin:$PATH"
export PATH LLVM_CONFIG
if [ "$("$LLVM_CONFIG" --version | cut -d. -f1)" != 22 ]; then
    echo 'The verified AVR compiler adapter requires LLVM22.' >&2
    exit 2
fi
: "${CARGO:=$HOME/.cargo/bin/cargo}"
: "${RUSTC:=$HOME/.cargo/bin/rustc}"
: "${CLANGXX:=$llvm_bin/clang++}"
: "${LLVM_AR:=$llvm_bin/llvm-ar}"
: "${LLVM_OBJDUMP:=$llvm_bin/llvm-objdump}"
: "${LLVM_OBJCOPY:=$llvm_bin/llvm-objcopy}"
: "${AVR_RUNTIME_ROOT:=$HOME/tmp/openrigcue-v6-libc/arduino7/avr}"
export CARGO RUSTC CLANGXX LLVM_AR LLVM_OBJDUMP LLVM_OBJCOPY AVR_RUNTIME_ROOT
