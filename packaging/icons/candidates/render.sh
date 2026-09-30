#!/usr/bin/env bash
# Render the icon candidates for review with the same rasterizer as ../render.sh.
# macOS only (AppKit). Leaves the shipping icon alone.
#
#   png/<name>-<size>.png   1024 to 16 px, with the macOS 10% margin (what the Dock shows)
#   contact-sheet.png       every candidate plus the current icon, dark and light panels
set -euo pipefail

cd "$(dirname "$0")"

if [[ "$(uname -s)" != Darwin ]]; then
    echo "render.sh needs macOS (AppKit)." >&2
    exit 1
fi

MAC_INSET=0.098 # Same margin ../render.sh uses for the .icns.
names=(pulse-s port-signal terminal-window glass-chevron)
labels=("Pulse S" "Port and signal" "Terminal window" "Glass chevron")

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

swiftc -O ../render-svg.swift -o "$tmp/render-svg"
swiftc -O contact-sheet.swift -o "$tmp/contact-sheet"

mkdir -p png
for name in "${names[@]}"; do
    for size in 1024 512 256 128 64 32 16; do
        "$tmp/render-svg" "$name.svg" "png/$name-$size.png" "$size" "$MAC_INSET"
    done
done

# The shipping icon as a reference row; these renders are not kept.
for size in 256 64 32 16; do
    "$tmp/render-svg" ../serialist.svg "$tmp/current-$size.png" "$size" "$MAC_INSET"
done

rows=()
for i in "${!names[@]}"; do
    rows+=("png/${names[$i]}=${labels[$i]}")
done
"$tmp/contact-sheet" contact-sheet.png "${rows[@]}" "$tmp/current=Current (reference)"

ls -l contact-sheet.png png
