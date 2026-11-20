#!/bin/bash
# Build inert ELF fixtures; inspect their metadata without executing them.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fixture=$(mktemp -d /tmp/openrigcue-glibc-fixture.XXXXXX)
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture/stage/nested plugins" "$fixture/libraries"
cat > "$fixture/export.c" <<'C'
void future(void) {}
C
cat > "$fixture/strong.c" <<'C'
extern void future(void);
void call(void) { future(); }
C
cat > "$fixture/weak.c" <<'C'
extern void future(void) __attribute__((weak));
void call(void) { if (future) future(); }
C
printf 'GLIBC_9.99 { global: future; };\n' > "$fixture/versions"
cc -shared -fPIC -nostdlib "$fixture/export.c" -Wl,--version-script="$fixture/versions" -o "$fixture/libraries/future.so"
cc -shared -fPIC -nostdlib "$fixture/strong.c" "$fixture/libraries/future.so" -o "$fixture/stage/nested plugins/future.bin"
checker=$root/packaging/appimage/check-glibc.pl
if perl "$checker" "$fixture/stage" 2.35 "$fixture/rejected.json" > "$fixture/rejected.log" 2>&1; then
    echo 'accepted a newer nested ELF requirement' >&2; exit 1
fi
perl -MJSON::PP -e 'local $/; $x=decode_json(<>);die unless !$x->{passed} && $x->{elf_count}==1 && $x->{required_maximum_glibc} eq "9.99" && @{$x->{violations}}' "$fixture/rejected.json"
perl "$checker" "$fixture/stage" 9.99 "$fixture/passed.json"
# A provider exporting a new version has no dependency on that version.
cp "$fixture/libraries/future.so" "$fixture/stage/provider.so"
rm "$fixture/stage/nested plugins/future.bin"
printf 'MZnot-an-ELF\n' > "$fixture/stage/bridge.exe"
perl "$checker" "$fixture/stage" 2.35 "$fixture/provider.json"
perl -MJSON::PP -e 'local $/; $x=decode_json(<>);die unless $x->{passed} && $x->{elf_count}==1 && !defined $x->{required_maximum_glibc}' "$fixture/provider.json"
# ABI marker tags are version needs without a normal symbol import.
printf 'GLIBC_ABI_GNU2_TLS { global: future; };\n' > "$fixture/versions"
cc -shared -fPIC -nostdlib "$fixture/export.c" -Wl,--version-script="$fixture/versions" -o "$fixture/libraries/future.so"
cc -shared -fPIC -nostdlib "$fixture/strong.c" "$fixture/libraries/future.so" -o "$fixture/stage/abi.so"
if perl "$checker" "$fixture/stage" 2.42 "$fixture/abi-rejected.json" > /dev/null 2>&1; then
    echo 'accepted ABI tag without guaranteed release support' >&2; exit 1
fi
perl "$checker" "$fixture/stage" 2.43 "$fixture/abi-passed.json"
# A weak symbol can still carry a non-weak loader version requirement.
cc -shared -fPIC -nostdlib "$fixture/weak.c" "$fixture/libraries/future.so" -o "$fixture/stage/weak.so"
rm "$fixture/stage/abi.so"
if perl "$checker" "$fixture/stage" 2.35 "$fixture/weak.json" > /dev/null 2>&1; then
    echo 'ignored non-weak loader version requirement' >&2; exit 1
fi
perl -MJSON::PP -e 'local $/; $x=decode_json(<>);die unless !$x->{passed} && $x->{elf_count}==2 && $x->{required_maximum_glibc} eq "2.43"' "$fixture/weak.json"
if perl "$checker" "$fixture/stage" invalid "$fixture/invalid.json" > /dev/null 2>&1; then
    echo 'accepted an invalid declared version' >&2; exit 1
fi
printf '7 inert ELF baseline cases passed; no fixture executed\n'
