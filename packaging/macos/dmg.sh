#!/usr/bin/env bash
# Wrap Serialist.app in a compressed disk image with an Applications shortcut.
#
#   dmg.sh <Serialist.app> <output.dmg> [version]
#
# Plain `hdiutil`, no Finder scripting, so it works on headless CI runners. The image has
# no custom background or icon layout: the app and an Applications link, ready to drag.
# When CODESIGN_IDENTITY is set to a real identity the image is signed too.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
source "$here/../lib.sh"

[[ "$(host_os)" == macos ]] || die "dmg.sh uses hdiutil and must run on macOS"
[[ $# -ge 2 ]] || die "usage: dmg.sh <Serialist.app> <output.dmg> [version]"

app="$1"
dmg="$2"
version="${3:-$(workspace_version)}"
identity="${CODESIGN_IDENTITY:--}"

[[ -d "$app" ]] || die "no app bundle at $app (run bundle.sh first)"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

# ditto keeps the bundle's extended attributes and its signature intact.
ditto "$app" "$stage/Serialist.app"
ln -s /Applications "$stage/Applications"

mkdir -p "$(dirname "$dmg")"
rm -f "$dmg"

# hdiutil create fails now and then with "Resource busy" on CI runners; a retry fixes it.
log "creating $dmg"
for attempt in 1 2 3; do
    if hdiutil create -volname "Serialist $version" -srcfolder "$stage" -fs HFS+ \
        -format UDZO -imagekey zlib-level=9 -ov "$dmg" >&2; then
        break
    fi
    [[ "$attempt" -lt 3 ]] || die "hdiutil create failed three times"
    log "hdiutil create failed (attempt $attempt), retrying"
    sleep 5
done

if [[ "$identity" != "-" ]]; then
    log "signing the disk image with \"$identity\""
    codesign --force --sign "$identity" --timestamp "$dmg"
fi

hdiutil verify -quiet "$dmg" >&2
log "built $dmg"
