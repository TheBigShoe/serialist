#!/usr/bin/env bash
# Regenerate every icon file from serialist.svg (and serialist-small.svg for the tiniest
# sizes). macOS only: AppKit rasterizes the SVG (render-svg.swift) and iconutil builds the
# .icns. Needs the Xcode command line tools and python3 (standard library only, for the .ico).
#
# Outputs, all committed so packaging on Linux and Windows needs no SVG renderer:
#   serialist.icns        macOS app icon (Apple's 10% transparent margin)
#   serialist.ico         Windows icon, 16 to 256 px
#   png/serialist-N.png   Linux hicolor sizes (16 to 512 px)
#
# Run it after editing serialist.svg or serialist-small.svg (`just icons`) and commit the
# results.
set -euo pipefail

cd "$(dirname "$0")"

if [[ "$(uname -s)" != Darwin ]]; then
    echo "render.sh needs macOS (AppKit and iconutil); the generated files are committed." >&2
    exit 1
fi

MAC_INSET=0.098 # 824 of 1024 px, the margin macOS icons are drawn with.
FLAT_INSET=0.03 # Windows and Linux icons sit closer to the edge.
# At 24 px and under the soft glow and the graticule of serialist.svg fill the counters of
# the S, so those sizes come from serialist-small.svg: the same icon with a thicker beam
# and no halo. 32 px and up are fine with the full artwork.
SMALL_MAX=24

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

swiftc -O render-svg.swift -o "$tmp/render-svg"
render() {
    local source=serialist.svg
    (("$2" <= SMALL_MAX)) && source=serialist-small.svg
    "$tmp/render-svg" "$source" "$1" "$2" "$3"
}

# macOS: an iconset of 16 to 512 px, each with an @2x, folded into one .icns.
iconset="$tmp/serialist.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
    render "$iconset/icon_${size}x${size}.png" "$size" "$MAC_INSET"
    render "$iconset/icon_${size}x${size}@2x.png" "$((size * 2))" "$MAC_INSET"
done
iconutil --convert icns "$iconset" --output serialist.icns

# Linux and Windows: rendered per size from the vector so small sizes stay crisp.
rm -rf png
mkdir png
for size in 16 24 32 48 64 128 256 512; do
    render "png/serialist-$size.png" "$size" "$FLAT_INSET"
done
python3 make-ico.py serialist.ico \
    png/serialist-16.png png/serialist-24.png png/serialist-32.png png/serialist-48.png \
    png/serialist-64.png png/serialist-128.png png/serialist-256.png

ls -l serialist.icns serialist.ico png
