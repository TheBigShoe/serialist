#!/usr/bin/env bash
# Build a release binary and package it for the current OS. This is the one entry point:
# `just release-<os>` and .github/workflows/release.yml both call it.
#
#   packaging/package.sh [macos|linux|windows] [--no-build] [--out DIR]
#
# With no OS argument it packages for the machine it runs on. Naming another OS is only
# a check that you meant this one: none of these packagers cross-compile.
#
#   macos    dist/Serialist-<version>-macos-<arch>.dmg    (and target/bundle/Serialist.app; arch is arm64 or x86_64)
#   linux    dist/serialist-<version>-linux-<arch>.tar.gz and dist/serialist_<version>-1_<arch>.deb
#   windows  dist/Serialist-<version>-windows-<arch>.msi
#
# Options:
#   --no-build   package the existing target/release binary instead of building it
#   --out DIR    where the finished packages go (default: dist/)
#
# Tools, beyond the pinned Rust toolchain:
#   macos    Xcode command line tools (codesign, hdiutil, plutil)
#   linux    cargo-deb   (cargo install cargo-deb --version 3.8.0 --locked)
#   windows  cargo-wix   (cargo install cargo-wix --version 0.3.9 --locked) and WiX Toolset 3.x
#
# Environment: CODESIGN_IDENTITY (macOS only) signs with a Developer ID instead of ad hoc.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$here/lib.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed -e '$d' -e 's/^# \{0,1\}//'; }

os="$(host_os)"
build=1
dist="$REPO_ROOT/dist"
requested=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        macos | linux | windows) requested="$1" ;;
        --no-build) build=0 ;;
        --out)
            [[ $# -ge 2 ]] || die "--out needs a directory"
            dist="$(mkdir -p "$2" && cd "$2" && pwd)"
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
done

if [[ -n "$requested" && "$requested" != "$os" ]]; then
    die "asked to package for $requested but this machine is $os; run the recipe on a $requested machine"
fi

cd "$REPO_ROOT"
version="$(workspace_version)"
arch="$(host_arch)"
[[ -n "$version" ]] || die "could not read the workspace version from Cargo.toml"
binary="$(release_binary)"
mkdir -p "$dist"

if [[ "$build" == 1 ]]; then
    log "building the release binary (cargo build --release --locked -p serialist)"
    cargo build --release --locked -p serialist
fi
[[ -x "$binary" ]] || die "no release binary at $binary (drop --no-build, or run: cargo build --release -p serialist)"

# The binary must at least start and report the version the packages are named for.
reported="$("$binary" --version)"
[[ "$reported" == "serialist $version" ]] \
    || die "the binary reports \"$reported\" but the workspace version is $version; rebuild it"
log "packaging $reported for $os-$arch"

package_macos() {
    local bundle_dir dmg mac_arch="$arch"
    # Apple calls it arm64.
    [[ "$arch" == aarch64 ]] && mac_arch=arm64
    bundle_dir="$(target_dir)/bundle"
    mkdir -p "$bundle_dir"
    "$here/macos/bundle.sh" "$binary" "$bundle_dir" "$version"
    dmg="$dist/Serialist-$version-macos-$mac_arch.dmg"
    "$here/macos/dmg.sh" "$bundle_dir/Serialist.app" "$dmg" "$version"
}

package_linux() {
    "$here/linux/tarball.sh" "$binary" "$dist" "$version"

    command -v cargo-deb >/dev/null 2>&1 \
        || die "cargo-deb is not installed: cargo install cargo-deb --version 3.8.0 --locked"
    log "building the .deb with cargo-deb (metadata in crates/serialist/Cargo.toml)"
    cargo deb -p serialist --no-build --output "$dist/"

    if command -v dpkg-deb >/dev/null 2>&1; then
        local deb
        for deb in "$dist"/serialist_*.deb; do
            dpkg-deb --info "$deb" >&2
            dpkg-deb --contents "$deb" >&2
        done
    fi
}

package_windows() {
    command -v cargo-wix >/dev/null 2>&1 \
        || die "cargo-wix is not installed: cargo install cargo-wix --version 0.3.9 --locked"

    # main.wxs reads the repository root from the environment, as a Windows path.
    if command -v cygpath >/dev/null 2>&1; then
        SERIALIST_ROOT="$(cygpath -w "$REPO_ROOT")"
        msi="$(cygpath -w "$dist")\\Serialist-$version-windows-$arch.msi"
    else
        SERIALIST_ROOT="$REPO_ROOT"
        msi="$dist/Serialist-$version-windows-$arch.msi"
    fi
    export SERIALIST_ROOT

    # An MSI version is major.minor.build; drop any pre-release suffix from the tag.
    log "building the .msi with cargo-wix (metadata in crates/serialist/Cargo.toml)"
    cargo wix -p serialist --no-build --nocapture \
        --install-version "$(numeric_version "$version")" --output "$msi"
}

case "$os" in
    macos) package_macos ;;
    linux) package_linux ;;
    windows) package_windows ;;
esac

log "done; packages in $dist"
(cd "$dist" && sha256 -- * 2>/dev/null | sed 's/^/    /' >&2) || true
ls -lh "$dist" >&2
