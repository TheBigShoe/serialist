#!/bin/sh
# Install Serialist from an unpacked release tarball.
#
#   ./install.sh                  # into ~/.local (no root needed)
#   PREFIX=/usr/local sudo ./install.sh
#   ./install.sh --uninstall      # remove what a previous run installed
#
# The tarball is laid out like a prefix (bin/, share/), so this only copies it into place.
# Serialist needs the system libraries listed in the README (Vulkan, Wayland or X11,
# xkbcommon, fontconfig, udev) and access to serial ports: add yourself to the group that
# owns /dev/ttyUSB* (usually dialout or uucp) and log in again.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$prefix/bin/serialist" "$prefix/share/applications/serialist.desktop"
    rm -f "$prefix"/share/icons/hicolor/*/apps/serialist.png
    rm -f "$prefix/share/icons/hicolor/scalable/apps/serialist.svg"
    rm -rf "$prefix/share/doc/serialist"
    echo "Removed Serialist from $prefix"
    exit 0
fi

mkdir -p "$prefix"
# cp -R over each top-level directory keeps the layout and merges into existing trees.
cp -R "$here/bin" "$here/share" "$prefix/"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "Installed Serialist to $prefix"
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "Note: $prefix/bin is not on your PATH." ;;
esac
