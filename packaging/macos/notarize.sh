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
# pass Gatekeeper offline. A submission the service rejects fails the script after
# printing the service's log, which names each problem (an unsigned binary, a missing
# secure timestamp, no hardened runtime).
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

credentials=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD")

# Notarization only accepts code signed with a Developer ID, the hardened runtime and a
# secure timestamp. Checking here saves a round trip to the service.
codesign --verify --strict --verbose=2 "$dmg" >&2 \
    || die "$dmg is not signed; set CODESIGN_IDENTITY when packaging"

log "submitting $dmg to the notary service (this waits for the verdict)"
# The verdict is read from the JSON rather than from the exit status, so a rejected
# submission is reported the same way whatever notarytool's exit status for it is.
result="$(xcrun notarytool submit "$dmg" "${credentials[@]}" --wait --timeout 1h \
    --output-format json)" || true
printf '%s\n' "$result" >&2
id="$(printf '%s' "$result" | plutil -extract id raw -o - - 2>/dev/null || true)"
status="$(printf '%s' "$result" | plutil -extract status raw -o - - 2>/dev/null || true)"

if [[ "$status" != Accepted ]]; then
    if [[ -n "$id" ]]; then
        log "the notary service says \"$status\"; its log for submission $id:"
        xcrun notarytool log "$id" "${credentials[@]}" >&2 || true
    fi
    die "notarization of $dmg did not succeed (status: ${status:-unknown})"
fi

log "accepted (submission $id); stapling the ticket"
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
