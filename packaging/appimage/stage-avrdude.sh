#!/bin/bash
# Stage the build distro's native firmware tool; no device access or flashing.
set -euo pipefail
appdir=${1:?AppDir path required}
helper="$appdir/usr/lib/openrigcue"
notices="$appdir/usr/share/licenses/openrigcue/avrdude"
install -Dm0755 /usr/bin/avrdude "$helper/avrdude-bin"
install -Dm0755 packaging/appimage/avrdude "$helper/avrdude"
install -Dm0644 /etc/avrdude.conf "$appdir/usr/share/openrigcue/avrdude.conf"
mkdir -p "$helper/avrdude-libs" "$notices"

declare -A packages=([avrdude]=1)
# ldd includes transitive dependencies. Only the base C runtime stays on the host;
# the copied libraries are searched solely by this helper's wrapper.
dependencies=$(LC_ALL=C ldd /usr/bin/avrdude)
if [[ "$dependencies" == *'not found'* ]]; then
    echo 'avrdude has an unresolved build dependency' >&2
    exit 1
fi
while read -r library; do
    name=$(basename "$library")
    case "$name" in
        libc.so.*|libm.so.*|libpthread.so.*|libdl.so.*|librt.so.*|ld-linux*.so.*) continue ;;
    esac
    install -m0644 "$library" "$helper/avrdude-libs/$name"
    owner=$(dpkg-query -S "$library" 2>/dev/null || dpkg-query -S "$(readlink -f "$library")")
    # Package names may carry an architecture suffix, e.g. libusb-1.0-0:amd64.
    package=${owner%%: /*}
    packages["$package"]=1
done < <(awk '$2 == "=>" && $3 ~ /^\// { print $3 }' <<<"$dependencies" | sort -u)

for package in "${!packages[@]}"; do
    name=${package%%:*}
    install -Dm0644 "/usr/share/doc/$name/copyright" "$notices/$name-copyright"
done
mkdir -p "$notices/common-licenses"
cp -L /usr/share/common-licenses/* "$notices/common-licenses/"
dpkg-query -W -f='${Package} ${Version}\n' "${!packages[@]}" | LC_ALL=C sort > "$notices/packages.txt"
