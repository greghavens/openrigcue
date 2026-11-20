#!/bin/sh
# OpenRigCue installer for Linux (x86_64).
#
# Downloads the latest OpenRigCue AppImage to ~/.local/bin/openrigcue and adds OpenRigCue to the
# application menu. Run it again to update. Nothing is installed system-wide and no root access is used.
#
#   OPENRIGCUE_INSTALL_DIR   folder for the program (default: ~/.local/bin)
set -eu

release=https://github.com/greghavens/openrigcue/releases/latest/download/openrigcue-x86_64.AppImage
icon=https://raw.githubusercontent.com/greghavens/openrigcue/main/packaging/appimage/openrigcue.svg

fail() {
    echo "OpenRigCue was not installed: $*" >&2
    exit 1
}

[ "$(uname -s)" = Linux ] || fail "OpenRigCue runs on Linux."
case $(uname -m) in
    x86_64 | amd64) ;;
    *) fail "this computer is $(uname -m); OpenRigCue needs an x86_64 computer." ;;
esac
[ -n "${HOME:-}" ] || fail "HOME is not set."

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fL --progress-bar -o "$2" "$1"; }
    fetch_quiet() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q --show-progress -O "$2" "$1"; }
    fetch_quiet() { wget -q -O "$2" "$1"; }
else
    fail "curl or wget is needed to download it."
fi

bin_dir=${OPENRIGCUE_INSTALL_DIR:-$HOME/.local/bin}
data_dir=${XDG_DATA_HOME:-$HOME/.local/share}
app=$bin_dir/openrigcue
icon_dir=$data_dir/icons/hicolor/scalable/apps
menu_dir=$data_dir/applications

mkdir -p "$bin_dir" "$icon_dir" "$menu_dir"

download=$app.download.$$
trap 'rm -f "$download"' EXIT
echo "Downloading OpenRigCue..."
fetch "$release" "$download" || fail "the download failed. Check your internet connection and try again."
chmod +x "$download"
mv -f "$download" "$app"

# The menu entry and its icon; OpenRigCue still works from a terminal if these cannot be written.
# The entry names the icon file itself, so the menu shows it without waiting for an icon theme refresh.
# StartupWMClass matches the window to this entry, which gives the running window its icon.
icon_file=$icon_dir/openrigcue.svg
fetch_quiet "$icon" "$icon_file" 2>/dev/null || true
if [ -s "$icon_file" ]; then menu_icon=$icon_file; else menu_icon=openrigcue; fi
cat > "$menu_dir/openrigcue.desktop" <<ENTRY
[Desktop Entry]
Type=Application
Name=OpenRigCue
Comment=Motion cueing and sim effects for motion platforms, shakers, wind and LEDs
Exec="$app"
Icon=$menu_icon
Terminal=false
Categories=Game;Simulation;
StartupWMClass=openrigcue
ENTRY

# A login-start entry left by an earlier copy of OpenRigCue is pointed at this one; its other settings,
# such as being switched off, are kept.
autostart=${XDG_CONFIG_HOME:-$HOME/.config}/autostart/openrigcue.desktop
if [ -f "$autostart" ] && ! grep -q "^Exec=\"$app\"" "$autostart"; then
    escaped=$(printf '%s' "$app" | sed 's/[\\&|]/\\&/g')
    sed -i "s|^Exec=\"[^\"]*\"|Exec=\"$escaped\"|; s|^Exec=[^\" ][^ ]*|Exec=\"$escaped\"|" "$autostart" || true
fi

# Refresh the desktop's menu and icon caches so the entry and icon appear without logging out.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$menu_dir" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$data_dir/icons/hicolor" >/dev/null 2>&1 || true
fi
for rebuild in kbuildsycoca6 kbuildsycoca5; do
    if command -v "$rebuild" >/dev/null 2>&1; then
        "$rebuild" >/dev/null 2>&1 || true
        break
    fi
done
touch "$data_dir/icons/hicolor" 2>/dev/null || true

echo
echo "OpenRigCue is installed."
echo "Start it from your application menu, or run: $app"
case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) echo "Tip: add $bin_dir to your PATH to start it by typing 'openrigcue'." ;;
esac
