#!/usr/bin/env bash
# Build the Linux release tarball: the binary, the .desktop entry, hicolor icons, the
# licenses and an install script, laid out like an install prefix.
#
#   tarball.sh <release binary> <output dir> [version]
#
# Writes <output dir>/serialist-<version>-linux-<arch>.tar.gz. There is no AppImage: its
# point is to bundle the shared libraries, and the GPU stack (Vulkan loader, Wayland,
# xkbcommon, fontconfig) is exactly what must come from the host to match its drivers.
# The tarball and the .deb use the host's libraries and say which ones they need.
#
# The script only copies files, so it also runs on macOS, which is how it is tested there.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
source "$here/../lib.sh"

[[ $# -ge 2 ]] || die "usage: tarball.sh <release binary> <output dir> [version]"

binary="$1"
out="$2"
version="${3:-$(workspace_version)}"
arch="${TARBALL_ARCH:-$(host_arch)}"

[[ -x "$binary" ]] || die "no executable at $binary (build it with: cargo build --release -p serialist)"
[[ -n "$version" ]] || die "could not read the workspace version"

name="serialist-$version-linux-$arch"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/$name"

log "assembling $name"
mkdir -p "$root/bin" "$root/share/applications" "$root/share/doc/serialist" \
    "$root/share/icons/hicolor/scalable/apps"
install -m 755 "$binary" "$root/bin/serialist"
install -m 644 "$here/serialist.desktop" "$root/share/applications/serialist.desktop"
install -m 755 "$here/install.sh" "$root/install.sh"

for size in 16 24 32 48 64 128 256 512; do
    dir="$root/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    install -m 644 "$here/../icons/png/serialist-$size.png" "$dir/serialist.png"
done
install -m 644 "$here/../icons/serialist.svg" "$root/share/icons/hicolor/scalable/apps/serialist.svg"

for doc in LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md; do
    [[ -f "$REPO_ROOT/$doc" ]] && install -m 644 "$REPO_ROOT/$doc" "$root/share/doc/serialist/$doc"
done

mkdir -p "$out"
archive="$out/$name.tar.gz"
rm -f "$archive"

# Owner and group 0 keep the archive from carrying the builder's user name. GNU tar and
# bsdtar spell the options differently.
tar_args=()
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
    tar_args=(--owner=0 --group=0 --numeric-owner)
else
    tar_args=(--uid 0 --gid 0 --numeric-owner)
fi
# COPYFILE_DISABLE stops macOS tar from adding AppleDouble (._*) entries.
COPYFILE_DISABLE=1 tar "${tar_args[@]}" -czf "$archive" -C "$stage" "$name"

log "built $archive"
