# Releasing Serialist

A release is a `v*` tag. Pushing it makes `.github/workflows/release.yml` build the
packages on macOS, Linux and Windows and attach them to a **draft** GitHub Release. You try
the packages, then publish the draft. The packages are unsigned (see [Unsigned
builds](#unsigned-builds)).

## What a release contains

| OS | File | Made by |
| --- | --- | --- |
| macOS (Apple Silicon) | `Serialist-<version>-macos-arm64.dmg`, holding `Serialist.app` | `packaging/macos/bundle.sh` and `dmg.sh` |
| Linux (x86_64) | `serialist-<version>-linux-x86_64.tar.gz` | `packaging/linux/tarball.sh` |
| Linux (x86_64) | `serialist_<version>-1_amd64.deb` | cargo-deb, from `[package.metadata.deb]` in `crates/serialist/Cargo.toml` |
| Windows (x86_64) | `Serialist-<version>-windows-x86_64.msi` | cargo-wix, from `packaging/windows/main.wxs` and `[package.metadata.wix]` |
| all | `SHA256SUMS` | the `release` job |

Every package carries `THIRD_PARTY_LICENSES.md`, the copyright and MIT license text of the
bundled Fadetouched theme (a license condition: the notice must travel with the program, and
the theme is compiled into the binary). It lands in `Serialist.app/Contents/Resources/`
(`bundle.sh`), in `share/doc/serialist/` in the tarball (`tarball.sh`) and the `.deb` (the
`assets` list in `crates/serialist/Cargo.toml`), and next to the program as
`THIRD_PARTY_LICENSES.txt` in the `.msi` (`packaging/windows/main.wxs`). `tarball.sh` and
`bundle.sh` fail if the file is missing. When you bundle another third-party file, add its
notice to that file and check that it still ships.

There is no AppImage. Its point is to bundle shared libraries, and the GPU stack (Vulkan
loader, Wayland, xkbcommon, fontconfig) has to come from the host to match its drivers. The
tarball and the `.deb` use the host's libraries instead. The tarball unpacks like an
install prefix (`bin/`, `share/`) and has an `install.sh` that copies it to `~/.local`.

## Cutting a release

1. **Start from a green `main`.** CI (`.github/workflows/ci.yml`) must pass on the commit
   you are releasing.
2. **Bump the version.** Set `version` under `[workspace.package]` in the root `Cargo.toml`
   (every crate inherits it), then run `cargo check --workspace` so `Cargo.lock` follows.
   Pre-release versions such as `0.2.0-rc.1` work: the release is marked as a pre-release,
   and the macOS and Windows metadata use the plain `0.2.0`.
3. **Update `CHANGELOG.md`.** Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a
   fresh empty `## [Unreleased]` above it. That section becomes the top of the release
   notes, ahead of the install instructions and GitHub's generated list of changes.
4. **Commit and tag.**

   ```sh
   git commit -am "Release X.Y.Z"
   git tag -a vX.Y.Z -m "Serialist X.Y.Z"
   git push origin main vX.Y.Z
   ```

   The tag must equal `v` plus the version in `Cargo.toml`. The workflow checks this first
   and fails fast if you tagged before bumping.
5. **Watch the workflow** (Actions, Release). Three `package` jobs run in parallel, each
   building the release binary (the first run is slow: GPUI compiles from scratch), running
   `--version` on it, and packaging. When all three pass, the `draft release` job downloads
   their artifacts, adds `SHA256SUMS` and creates the draft.
6. **Try the drafts.** Download each package on its OS and start it. On macOS use the steps
   under [Unsigned builds](#unsigned-builds). Check that the app icon shows and the app opens
   a window. Read the notes.
7. **Publish** the draft (Releases, Edit, Publish release). The tag already exists, so
   publishing does not move it.

If a job fails, fix the cause and use "Re-run failed jobs", or, if the fix needs a new
commit, delete the draft release and the tag (`git push --delete origin vX.Y.Z`,
`git tag -d vX.Y.Z`) and tag again. Re-running for an existing draft replaces its files.

## Trying the workflow without a tag

Run it by hand: Actions, Release, Run workflow. That builds and packages on all three
systems and keeps the results as workflow artifacts for 14 days. Tick `draft_release` to
also create the draft. GitHub only offers a manual run for a workflow file that exists on
the default branch, so do this once after merging the packaging work and before the first
real tag.

## Building packages locally

Each recipe builds the release binary and writes to `dist/`. None of them cross-compile.

```sh
just release-macos     # target/bundle/Serialist.app and dist/Serialist-<version>-macos-<arch>.dmg
just release-linux     # dist/*.tar.gz and dist/*.deb   (cargo install cargo-deb --version 3.8.0 --locked)
just release-windows   # dist/*.msi   (cargo install cargo-wix --version 0.3.9 --locked, and WiX Toolset 3)
just release           # whichever fits this machine
```

They are thin wrappers around `packaging/package.sh [macos|linux|windows] [--no-build]
[--out DIR]`, which is also what CI runs. On Windows it needs bash (Git Bash) and `just`
uses it too. Useful checks on the macOS output:

```sh
plutil -lint target/bundle/Serialist.app/Contents/Info.plist
codesign -dv --verbose=2 target/bundle/Serialist.app
open -n target/bundle/Serialist.app --args --virtual echo   # runs from the bundle
```

Run from the bundle, the app has a bundle identifier (`com.serialist.app`), so system
notifications are enabled and the log no longer says "system notifications disabled: not
running from an app bundle".

## Unsigned builds

**macOS.** `bundle.sh` signs the app ad hoc (`codesign --sign -`). That is enough for the
binary to run on Apple Silicon and gives the bundle a valid, sealed signature, but it is not
an identified developer, so Gatekeeper blocks a downloaded copy. To open it: Control-click
Serialist in Applications, choose Open, and confirm (macOS 14 and earlier); or try to open it
once, then press Open Anyway in System Settings, Privacy & Security (macOS 15 and later); or
run `xattr -dr com.apple.quarantine /Applications/Serialist.app`. A copy built on the same
Mac is not quarantined and opens normally.

**Windows.** SmartScreen shows "Windows protected your PC": More info, then Run anyway.

**Turning on macOS signing and notarization** needs a paid Apple Developer account:

1. Create a "Developer ID Application" certificate and export it as a `.p12`.
2. Create an app-specific password at appleid.apple.com.
3. Add the repository secrets listed in the commented block in the `package` job of
   `release.yml`: `MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`,
   `MACOS_KEYCHAIN_PASSWORD`, `MACOS_SIGN_IDENTITY`, `APPLE_ID`, `APPLE_TEAM_ID` and
   `APPLE_APP_PASSWORD`.
4. Uncomment the three marked pieces (import certificate, `CODESIGN_IDENTITY` on the build
   step, notarize step). `bundle.sh` and `dmg.sh` then sign with the identity, the hardened
   runtime and a secure timestamp when `CODESIGN_IDENTITY` is set, and
   `packaging/macos/notarize.sh` submits the `.dmg` with `notarytool --wait` and staples the
   ticket.
5. Remove the "unsigned" wording from `README.md`, `packaging/release-notes-install.md` and
   the header of `release.yml`.

None of the notarization path has been run: it needs credentials.

Windows code signing is not set up. When it is, `cargo wix sign` or `signtool` on the `.msi`
(and on `serialist.exe` before packaging) is the place to add it.

## Changing the icon

`packaging/icons/serialist.svg` is the source (the "Pulse S" mark). Edit it and run
`just icons` (macOS only: it renders with AppKit and builds the `.icns` with `iconutil`).
That rewrites `serialist.icns`, `serialist.ico` and `png/serialist-*.png`, which are
committed so that packaging on Linux and Windows needs no SVG renderer. Commit the
regenerated files.

Renders of 24 px and under (the 16 and 24 px PNGs and `.ico` entries, and the 16 px
`.icns` slice) come from `serialist-small.svg`, the same icon with a tighter glow and a
thicker beam, because the full artwork's halo fills the counters of the S at that size.
Keep the two files in step when you change the shape or the colors. Both must stay free of
SVG filters, masks and patterns: the same `serialist.svg` is installed as the Linux
scalable icon, and desktop icon renderers do not all support them. `classic.svg` is the
previous mark, kept for history, and `candidates/` holds the other designs that were
considered; neither is used by the build.

On Windows, `crates/serialist/build.rs` embeds `serialist.ico` in `serialist.exe` as icon
resource 1 (the id GPUI's Windows backend loads), by writing a `.res` file and passing it to
the MSVC linker. It does nothing on other targets.

## Known gaps

- **macOS is arm64 only.** `macos-latest` is Apple Silicon. For an Intel build add a matrix
  row on an Intel runner (`macos-15-intel`, `macos-26-intel`).
- **The Linux glibc floor** is that of the build runner (`ubuntu-22.04`, glibc 2.35). The
  tarball and `.deb` link the host's shared libraries: the `.deb` depends on what
  `dpkg-shlibdeps` finds plus `libvulkan1` (loaded at run time); a tarball user installs the
  libraries named in the README.
- **Linux window icon.** Wayland compositors match a window to `serialist.desktop` by its
  app id, and the app does not set one yet. Setting `app_id: Some("serialist".into())` in the
  main window's options would make the taskbar icon show. The `.desktop` file already has
  `StartupWMClass=serialist`.
- **No version resource on Windows.** `serialist.exe` has an icon but no VERSIONINFO, so
  Explorer's Details tab is empty.
- **Windows on ARM and Linux arm64** are not built.
- **First run.** The macOS steps have been run. The Linux and Windows jobs were written on a
  Mac and have not run; expect to fix small things on the first manual run.
