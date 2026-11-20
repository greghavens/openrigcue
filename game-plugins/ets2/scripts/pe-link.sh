#!/bin/sh
# gcc -wrapper for the release DLL's link (build.rs passes
# `-wrapper <this script>`; gcc then runs every subprogram as
# `pe-link.sh <program> <args>`). Programs other than the linker run
# unchanged. The link itself runs with a link map, and the image is then given
# the header fields that GNU ld cannot produce:
#
#   COFF TimeDateStamp      fixed (x64 0x55f867da, i386 0x55f8677f),
#                           also in the export directory
#   COFF Characteristics    fixed (x64 0x2022, i386 0x2102): ld adds
#                           0x020c (line numbers / local symbols / debug
#                           stripped) to stripped images
#   Linker version          11.0
#   CheckSum                0 (ld always computes one)
#   Data directories        export (0), import (1), debug (6), load
#                           configuration (10, size 0x70 x64 / 0x40 i386) and
#                           IAT (12): ld fills these only from sections named
#                           .edata / .idata (and from __load_config_used only
#                           in later binutils), which ets2-telemetry.ld merges
#                           into .rdata
#   Debug entries           PointerToRawData of the two debug directory entries
#                           (file offsets, which the assembler cannot know)
#
# Everything is located through the link map (ets2-telemetry.ld and the
# symbols of src/imports_*.s / src/crt_*.s), never by searching the image.
set -eu
case "${1##*/}" in
collect2 | collect2.exe | ld | ld.bfd | *-ld | *-ld.bfd) ;;
*) exec "$@" ;;
esac

# rustc puts its rsbegin.o / rsend.o into every windows-gnu link. On i686
# they hold the libgcc DWARF frame registration (code, a .bss object and
# .ctors/.dtors entries calling __register_frame_info) that only the
# mingw-w64 CRT would run; this image has no use for that code or data, so
# they are taken out of the link (nothing in the plugin or crt.rs refers to
# them).
n=$#
while [ "$n" -gt 0 ]; do
    a=$1
    shift
    n=$((n - 1))
    case ${a##*/} in
    rsbegin.o | rsend.o) continue ;;
    esac
    set -- "$@" "$a"
done

out=
prev=
for a in "$@"; do
    [ "$prev" = -o ] && out=$a
    prev=$a
done
[ -n "$out" ] || { echo "pe-link.sh: no -o in the link command" >&2; exit 1; }
map=$out.map
"$@" "-Map=$map"

die() { echo "pe-link.sh: $out: $*" >&2; exit 1; }
u16() { od -An -tu2 -j "$1" -N2 "$out" | tr -d ' '; }
u32() { od -An -tu4 -j "$1" -N4 "$out" | tr -d ' '; }
# put <offset> <value> <bytes>: little-endian store.
put() {
    o=$1 v=$2 n=$3 esc=
    while [ "$n" -gt 0 ]; do
        esc="$esc\\$(printf '%03o' $((v & 0xff)))"
        v=$((v >> 8))
        n=$((n - 1))
    done
    # shellcheck disable=SC2059
    printf "$esc" | dd of="$out" bs=1 seek="$o" conv=notrunc status=none
}
# sym <name>: the symbol's address from the link map (as an input-section
# symbol line "addr name" or a script assignment "addr name = ."); the i386
# map shows C symbols without their leading underscore, so both spellings
# are tried.
sym() {
    for s in "$1" "_$1"; do
        v=$(awk -v s="$s" '$2 == s && (NF == 2 || $3 == "=") && $1 ~ /^0x/ { print $1; exit }' "$map")
        [ -n "$v" ] && { echo "$v"; return; }
    done
    die "symbol $1 not in $map"
}

pe=$(u32 60)
[ "$(u32 "$pe")" = 17744 ] || die "not a PE image" # "PE\0\0"
opt=$((pe + 24))
case $(u16 "$opt") in
523) # PE32+ (x64)
    dirs=$((opt + 112)) stamp=$((0x55f867da)) chars=$((0x2022)) loadcfg_size=$((0x70)) ;;
267) # PE32 (i386)
    dirs=$((opt + 96)) stamp=$((0x55f8677f)) chars=$((0x2102)) loadcfg_size=$((0x40)) ;;
*) die "unknown optional header magic" ;;
esac
base=$(($(sym __image_base__)))
nsect=$(u16 $((pe + 6)))
sects=$((opt + $(u16 $((pe + 20)))))

# rva2off <rva>: file offset of an RVA, from the section table.
rva2off() {
    i=0
    while [ "$i" -lt "$nsect" ]; do
        h=$((sects + 40 * i))
        va=$(u32 $((h + 12)))
        size=$(u32 $((h + 16)))
        if [ "$1" -ge "$va" ] && [ "$1" -lt $((va + size)) ]; then
            echo $(($1 - va + $(u32 $((h + 20)))))
            return
        fi
        i=$((i + 1))
    done
    die "RVA $1 is in no section"
}
# directory <index> <rva> <size>
directory() {
    put $((dirs + 8 * $1)) "$2" 4
    put $((dirs + 8 * $1 + 4)) "$3" 4
}

export_rva=$(($(sym ets2_export_directory) - base))
export_size=$(($(sym ets2_export_directory_end) - base - export_rva))
import_rva=$(($(sym ets2_import_directory) - base))
import_size=$(($(sym ets2_import_directory_end) - base - import_rva))
iat_rva=$(($(sym ets2_iat_start) - base))
iat_size=$(($(sym ets2_iat_end) - base - iat_rva))
debug_rva=$(($(sym ets2_debug_directory) - base))
debug_size=$(($(sym ets2_debug_directory_end) - base - debug_rva))
loadcfg_rva=$(($(sym _load_config_used) - base))
[ "$export_size" -gt 0 ] || die "empty export directory"

put $((pe + 8)) "$stamp" 4
put $((pe + 22)) "$chars" 2
put $((opt + 2)) 11 1
put $((opt + 3)) 0 1
put $((opt + 64)) 0 4
directory 0 "$export_rva" "$export_size"
directory 1 "$import_rva" "$import_size"
directory 6 "$debug_rva" "$debug_size"
directory 10 "$loadcfg_rva" "$loadcfg_size"
directory 12 "$iat_rva" "$iat_size"
# The export directory ld builds (.edata: the 0x28-byte directory, then the
# address, name pointer and ordinal tables and the names) holds RVAs computed
# as if .edata began its own output section; inside .rdata they are short by
# the directory's offset there. Move the directory's Name, AddressOfFunctions,
# AddressOfNames and AddressOfNameOrdinals and the name pointers by that much.
edata=$(rva2off "$export_rva")
delta=$((export_rva + 0x28 - $(u32 $((edata + 0x1c)))))
for f in 0x0c 0x1c 0x20 0x24; do
    put $((edata + f)) $(($(u32 $((edata + f))) + delta)) 4
done
names=$(u32 $((edata + 0x18)))
npt=$(rva2off "$(u32 $((edata + 0x20)))")
i=0
while [ "$i" -lt "$names" ]; do
    put $((npt + 4 * i)) $(($(u32 $((npt + 4 * i))) + delta)) 4
    i=$((i + 1))
done
put $((edata + 4)) "$stamp" 4
entry=$(rva2off "$debug_rva")
while [ "$debug_size" -gt 0 ]; do
    put $((entry + 24)) "$(rva2off "$(u32 $((entry + 20)))")" 4
    entry=$((entry + 28))
    debug_size=$((debug_size - 28))
done
