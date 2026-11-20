#!/usr/bin/env bash
# Fake privileged utilities record the deployment sequence; no host files change.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/bin"
cat > "$work/bin/install" <<'SH'
#!/bin/sh
printf 'install' >> "$OPENRIGCUE_INSTALL_LOG"
printf ' <%s>' "$@" >> "$OPENRIGCUE_INSTALL_LOG"
printf '\n' >> "$OPENRIGCUE_INSTALL_LOG"
SH
cat > "$work/bin/udevadm" <<'SH'
#!/bin/sh
printf 'udevadm' >> "$OPENRIGCUE_INSTALL_LOG"
printf ' <%s>' "$@" >> "$OPENRIGCUE_INSTALL_LOG"
printf '\n' >> "$OPENRIGCUE_INSTALL_LOG"
SH
chmod +x "$work/bin/install" "$work/bin/udevadm"
export OPENRIGCUE_INSTALL_LOG="$work/commands"
PATH="$work/bin:$PATH" sh "$root/packaging/udev/install-device-access" "$root/packaging/udev/70-openrigcue.rules"
cat > "$work/expected" <<EOF
install <-Dm0644> <--> <$root/packaging/udev/70-openrigcue.rules> </etc/udev/rules.d/70-openrigcue.rules>
udevadm <control> <--reload-rules>
udevadm <trigger> <--action=change> <--subsystem-match=tty>
udevadm <trigger> <--action=change> <--subsystem-match=hidraw>
udevadm <trigger> <--action=change> <--subsystem-match=usb>
EOF
cmp "$work/expected" "$work/commands"
if PATH="$work/bin:$PATH" sh "$root/packaging/udev/install-device-access" "$work/missing"; then exit 1; fi
cmp "$work/expected" "$work/commands"
printf 'device access installer: deployment order and missing-file rejection passed without host changes\n'
