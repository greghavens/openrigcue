#!/bin/bash
# Called before exec, before GTK/EGL or application worker threads exist.
# WebKit bug 324551: NVIDIA explicit sync can disconnect a GTK3 Wayland client
# while WebKit first paints. Keep the DMABUF renderer and honor user overrides.
openrigcue_apply_graphics_compatibility() {
    local nvidia_module_version=$1
    shift

    # Only a desktop window needs the compatibility environment. Do not interpret
    # or rewrite the arguments passed through to the actual CLI.
    local argument command=run skip_log_value=0
    for argument in "$@"; do
        case $argument in
            --headless|--usb-report|--usb-report=*|-h|--help|-V|--version) return 0 ;;
        esac
    done
    if (($#)); then
        command=
        for argument in "$@"; do
            if ((skip_log_value)); then
                skip_log_value=0
                continue
            fi
            case $argument in
                --log) skip_log_value=1 ;;
                --log=*) ;;
                -*) return 0 ;;
                *) command=$argument; break ;;
            esac
        done
        [[ $command == run || $command == tools ]] || return 0
    fi

    [[ -n ${WAYLAND_DISPLAY-} && -r $nvidia_module_version ]] || return 0
    local backends=${GDK_BACKEND-}
    if [[ -n $backends && ,$backends, != *,wayland,* && ,$backends, != *,\*,* ]]; then
        return 0
    fi
    # Presence matters: an explicitly empty value or 0 is a user override too.
    [[ ! -v __NV_DISABLE_EXPLICIT_SYNC ]] || return 0
    export __NV_DISABLE_EXPLICIT_SYNC=1
}
