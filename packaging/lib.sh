# shellcheck shell=bash
# Shared helpers for the scripts under packaging/. Source this file; do not run it.
#
#   source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
#
# Everything here works in bash on macOS, Linux and Windows (Git Bash, as installed on
# GitHub's Windows runners).

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

log() { printf '==> %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# macos, linux or windows.
host_os() {
    case "$(uname -s)" in
        Darwin) echo macos ;;
        Linux) echo linux ;;
        MINGW* | MSYS* | CYGWIN*) echo windows ;;
        *) die "unsupported OS: $(uname -s)" ;;
    esac
}

# x86_64 or aarch64, whatever the OS calls them.
host_arch() {
    case "$(uname -m)" in
        x86_64 | amd64 | AMD64) echo x86_64 ;;
        arm64 | aarch64 | ARM64) echo aarch64 ;;
        *) die "unsupported architecture: $(uname -m)" ;;
    esac
}

# The workspace version: the first `version = "..."` line of the root Cargo.toml, which
# is the one under [workspace.package]. Dependency versions are inline tables and never
# start a line with `version`.
workspace_version() {
    sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO_ROOT/Cargo.toml" | head -n 1
}

# The version without a pre-release or build suffix: 0.2.0-rc.1 becomes 0.2.0. Info.plist
# and MSI versions must be plain dotted numbers.
numeric_version() {
    local version="$1"
    echo "${version%%[-+]*}"
}

# Absolute path of the cargo target directory, honouring CARGO_TARGET_DIR.
target_dir() {
    echo "${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
}

# Path of the release binary for the host OS.
release_binary() {
    local exe=serialist
    [[ "$(host_os)" == windows ]] && exe=serialist.exe
    echo "$(target_dir)/release/$exe"
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$@"
    else
        shasum -a 256 "$@"
    fi
}
