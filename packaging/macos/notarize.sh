#!/usr/bin/env bash
# Notarize and staple a signed disk image. Needs an Apple Developer account, a Developer
# ID Application certificate (used earlier, through CODESIGN_IDENTITY, by bundle.sh and
# dmg.sh) and an app-specific password. Not run by default: see the commented notarization
# steps in .github/workflows/release.yml and docs/releasing.md.
#
#   APPLE_ID=you@example.com APPLE_TEAM_ID=ABCDE12345 APPLE_APP_PASSWORD=xxxx-xxxx-xxxx-xxxx \
#       notarize.sh <output.dmg>
#
# Notarizing the .dmg covers the .app inside it; stapling the ticket to the .dmg lets it
# pass Gatekeeper offline.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
source "$here/../lib.sh"

[[ "$(host_os)" == macos ]] || die "notarize.sh must run on macOS"
[[ $# -eq 1 ]] || die "usage: notarize.sh <output.dmg>"
dmg="$1"
[[ -f "$dmg" ]] || die "no disk image at $dmg"

: "${APPLE_ID:?set APPLE_ID to the Apple ID that owns the developer account}"
: "${APPLE_TEAM_ID:?set APPLE_TEAM_ID to the 10-character team ID}"
: "${APPLE_APP_PASSWORD:?set APPLE_APP_PASSWORD to an app-specific password}"

log "submitting $dmg to the notary service (this waits for the verdict)"
xcrun notarytool submit "$dmg" \
    --apple-id "$APPLE_ID" \
    --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" \
    --wait

log "stapling the ticket"
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
