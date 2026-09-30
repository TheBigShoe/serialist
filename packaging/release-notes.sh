#!/usr/bin/env bash
# Print the body for a GitHub Release: the CHANGELOG.md section for a version, then the
# install notes (including how to open the unsigned builds).
#
#   packaging/release-notes.sh <version>      e.g. 0.2.0, or Unreleased to preview
#
# The section is the text under the "## [<version>]" heading of CHANGELOG.md, up to the next
# "## " heading. With no such heading only the install notes are printed, and the release
# workflow adds GitHub's generated notes after them.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$here/lib.sh"

[[ $# -eq 1 ]] || die "usage: release-notes.sh <version>"
version="$1"
changelog="${CHANGELOG:-$REPO_ROOT/CHANGELOG.md}"

if [[ -f "$changelog" ]]; then
    awk -v version="$version" '
        /^## / {
            if (found) exit
            if (index($0, "[" version "]") == 4) found = 1
            next
        }
        found { print }
    ' "$changelog"
    echo
fi

cat "$here/release-notes-install.md"
