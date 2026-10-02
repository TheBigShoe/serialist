#!/usr/bin/env bash
# Assemble Serialist.app around a release binary and sign it.
#
#   bundle.sh <release binary> <output dir> [version]
#
# Writes <output dir>/Serialist.app. The version defaults to the workspace version.
#
# Signing: the bundle is signed ad hoc (identity "-"), which needs no Apple account and
# is enough for the binary to run on Apple Silicon, but Gatekeeper still treats a
# downloaded copy as unidentified. Set CODESIGN_IDENTITY to a "Developer ID Application:
# ..." identity to sign for distribution; that also turns on the hardened runtime and a
# secure timestamp, which notarization requires (see notarize.sh).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
source "$here/../lib.sh"

[[ "$(host_os)" == macos ]] || die "bundle.sh builds a macOS app bundle and must run on macOS"
[[ $# -ge 2 ]] || die "usage: bundle.sh <release binary> <output dir> [version]"

binary="$1"
out="$2"
version="$(numeric_version "${3:-$(workspace_version)}")"
identity="${CODESIGN_IDENTITY:--}"

[[ -x "$binary" ]] || die "no executable at $binary (build it with: cargo build --release -p serialist)"
[[ -n "$version" ]] || die "could not read the workspace version"

app="$out/Serialist.app"
log "assembling $app (version $version)"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

install -m 755 "$binary" "$app/Contents/MacOS/serialist"
sed "s/@VERSION@/$version/g" "$here/Info.plist.in" >"$app/Contents/Info.plist"
install -m 644 "$here/../icons/serialist.icns" "$app/Contents/Resources/serialist.icns"
# The third-party notices (the bundled Fadetouched theme's MIT license). Inside the bundle,
# so it is covered by the signature below and travels with the app out of the .dmg.
install -m 644 "$REPO_ROOT/THIRD_PARTY_LICENSES.md" "$app/Contents/Resources/THIRD_PARTY_LICENSES.md"
printf 'APPL????' >"$app/Contents/PkgInfo"

plutil -lint "$app/Contents/Info.plist" >&2

sign_args=(--force --sign "$identity" --identifier com.serialist.app)
if [[ "$identity" == "-" ]]; then
    log "signing ad hoc (set CODESIGN_IDENTITY to sign with a Developer ID)"
else
    log "signing with \"$identity\" (hardened runtime, secure timestamp)"
    sign_args+=(--options runtime --timestamp)
fi
codesign "${sign_args[@]}" "$app"

codesign --verify --strict --verbose=2 "$app" >&2
log "built $app"
