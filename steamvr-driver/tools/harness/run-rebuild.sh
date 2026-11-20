#!/bin/sh
# Run the harness scenarios against the Linux driver (driver_simracingstudio.so built with
# --features test-mirror-dir) and compare every capture with tests/reference.
#   usage: run-rebuild.sh <driver_simracingstudio.so> <out-dir>
# Named sections live in a per-case directory given by OPENRIGCUE_TEST_MIRROR_DIR (not /dev/shm).
# Masked before comparing: wall-clock times in log lines, Wine's own stderr line, the pointer value
# printed for an empty configuration value, and the inpage case's helper files (req<n>, ack<n>, rig.bin) under Wine.
# The poses of the failed inpage frames are not compared in full: they count as pose
# differences (the in-process test compares their result and poseIsValid bytes). Exit codes differ by platform (abort: 3 on Windows, 134 here).
set -eu
so=$(realpath "$1")
out=$(realpath -m "$2")
here=$(dirname "$(realpath "$0")")
ref=$(realpath "$here/../../tests/reference")
mkdir -p "$out"
g++ -O1 -o "$out/harness" "$here/harness.cpp" -ldl

run() { # case scenario [prep]
    w=$out/work/$1; m=$out/mirror/$1
    rm -rf "$w" "$m"; mkdir -p "$w" "$m"
    ( cd "$w"
      eval "${3:-:}"
      code=0
      OPENRIGCUE_TEST_MIRROR_DIR="$m" "$out/harness" "$2" "$w/trace.txt" "$so" >"$w/stdout.txt" 2>"$w/stderr.txt" || code=$?
      [ "$code" -ne 0 ] && echo "exit=$code" >"$w/exit.txt"
      true )
    o=$out/$1; rm -rf "$o"; mkdir -p "$o"
    cp "$w/trace.txt" "$w/stdout.txt" "$w/stderr.txt" "$o/"
    [ -f "$w/exit.txt" ] && cp "$w/exit.txt" "$o/"
    if [ -f "$w/driver_motionpose.log" ]; then
        sz=$(stat -c %s "$w/driver_motionpose.log"); echo "$sz" >"$o/size.txt"
        if [ "$sz" -lt 65536 ]; then cp "$w/driver_motionpose.log" "$o/log.txt"; else tail -c 400 "$w/driver_motionpose.log" >"$o/tail.txt"; fi
    fi
    ls "$w" >"$o/files.txt"
}
fill() { yes x | tr -d '\n' | head -c "$1" >driver_motionpose.log; }

run main main
run early early
run init-failures init-failures
run load load
run conf-valid load "printf '* GLOBAL:\n\tLOG_FLUSH_THRESHOLD = 2\n' >logging.conf"
run conf-crlf load "printf '* GLOBAL:\r\n\tLOG_FLUSH_THRESHOLD = 2\r\n* TRACE:\r\n\tFORMAT = \"%%msg\"\r\n' >logging.conf"
run conf-bad load "cp '$here/conf-bad.conf' logging.conf"
run conf-probe load "cp '$here/conf-probe.conf' logging.conf"
run conf-dir load "mkdir logging.conf"
run conf-empty load ": >logging.conf"
run roll-at-limit load "fill 2097152"
run roll-below-limit load "fill 2097151"
run log-append load "printf 'previous line\n' >driver_motionpose.log"
run inpage inpage
run crash-segv crash-segv
run crash-fpe crash-fpe
run crash-ill crash-ill
run crash-int crash-int

mask() { sed -E 's/(\[[A-Z ]{5}\]) [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}:/\1 <time>:/; s/(Empty configuration value for \[)[0-9A-F]{16}\]/\1<ptr>]/' "$1" | grep -v -E '^error: XDG_RUNTIME_DIR|^(req[0-9]|ack[0-9]|rig\.bin)$' || true; }
status=0
for c in $(ls "$ref"); do
    for f in $(ls "$ref/$c"); do
        case "$f" in exit.txt) continue ;; esac
        if [ ! -f "$out/$c/$f" ]; then echo "$c/$f: missing"; status=1; continue; fi
        if [ "$f" = trace.txt ]; then
            n=$(diff "$ref/$c/$f" "$out/$c/$f" | grep -c '^<' || true)
            other=$(diff "$ref/$c/$f" "$out/$c/$f" | grep '^[<>]' | grep -vc 'TrackedDevicePoseUpdated\|GetPose direct' || true)
            if [ "$n" -eq 0 ]; then echo "$c/$f: identical"; else echo "$c/$f: $n pose line(s) differ, $other other line(s) differ"; [ "$other" -eq 0 ] || status=1; fi
        elif cmp -s "$ref/$c/$f" "$out/$c/$f"; then
            echo "$c/$f: identical"
        else
            mask "$out/$c/$f" >"$out/$c/$f.masked"
            if mask "$ref/$c/$f" | cmp -s - "$out/$c/$f.masked"; then echo "$c/$f: identical (masked)"; else echo "$c/$f: DIFFERENT"; status=1; fi
        fi
    done
    [ -f "$ref/$c/exit.txt" ] && echo "$c: exit original $(cat "$ref/$c/exit.txt"), rebuild $(cat "$out/$c/exit.txt" 2>/dev/null || echo exit=0)"
done
exit $status
