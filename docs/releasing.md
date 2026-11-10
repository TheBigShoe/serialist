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
| macOS (Intel), extra | `Serialist-<version>-macos-x86_64.dmg` | as for Apple Silicon, on `macos-15-intel` |
| Linux (arm64), extra | `serialist-<version>-linux-aarch64.tar.gz` and `serialist_<version>-1_arm64.deb` | as for x86_64, on `ubuntu-22.04-arm` |
| all | `SHA256SUMS` | the `release` job |

The first four packages are required: the draft is made only when all of them are built.
The rows marked extra are `extra: true` in the workflow's matrix: a failure there shows as a
failed job, but the run does not fail and the draft is made without that package. Every
file name carries the architecture of the runner that built it (`host_arch` in
`packaging/lib.sh`; cargo-deb uses Debian's names), so an extra row never replaces a
required package, and `SHA256SUMS` and the upload take whatever files are there.

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
   (every crate inherits it), then run `cargo check --workspace` so `Cargo.lock` follows,
   and `cargo fetch --manifest-path fuzz/Cargo.toml` so the fuzz crate's own `Cargo.lock`
   follows too (it records the path crates' versions, and CI checks it with `--locked`).
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
5. **Watch the workflow** (Actions, Release). Five `package` jobs run in parallel, each
   building the release binary (the first run is slow: GPUI compiles from scratch), running
   `--version` on it, and packaging. When they are done, the `draft release` job downloads
   their artifacts, checks that the four required packages are there, adds `SHA256SUMS` and
   creates the draft. If an extra job failed, its package is missing from the draft: re-run
   it, or say in the notes that it is missing.
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
the default branch, so a change to `release.yml` or `packaging/` is tried this way after it
merges and before the next tag.

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
Windows code signing is not set up. When it is, `cargo wix sign` or `signtool` on the `.msi`
(and on `serialist.exe` before packaging) is the place to add it.

### Turning on macOS signing and notarization

This needs a paid Apple Developer Program membership. Only its Account Holder can create a
Developer ID certificate.

1. **Create the certificate** on the Mac you will export it from. Either Xcode, Settings,
   Accounts, select the team, Manage Certificates, `+`, "Developer ID Application"; or, at
   developer.apple.com, Account, Certificates, `+`, "Developer ID Application" (the G2
   Sub-CA), with a certificate signing request made in Keychain Access (Certificate
   Assistant, Request a Certificate From a Certificate Authority, saved to disk), then
   double-click the downloaded `.cer`. Either way the certificate and its private key end
   up in the login keychain. Not "Apple Development" or "Apple Distribution": those do not
   pass Gatekeeper outside the App Store.
2. **Find the identity name**: `security find-identity -v -p codesigning` prints it, as
   `Developer ID Application: <name> (<TEAMID>)`. The ten characters in parentheses are
   the team ID (also under Membership details at developer.apple.com).
3. **Export a `.p12`**: Keychain Access, login, My Certificates, expand the Developer ID
   Application certificate so its key shows, select the certificate, File, Export Items,
   Personal Information Exchange (`.p12`), with a new password.
4. **Make an app-specific password** for the Apple ID that owns the membership:
   account.apple.com, Sign-In and Security, App-Specific Passwords.
5. **Try it locally** before involving CI:

   ```sh
   CODESIGN_IDENTITY="Developer ID Application: <name> (<TEAMID>)" just release-macos
   codesign -dv --verbose=2 target/bundle/Serialist.app  # flags=0x10000(runtime), a Timestamp
   APPLE_ID=<email> APPLE_TEAM_ID=<TEAMID> APPLE_APP_PASSWORD=<app password> \
       packaging/macos/notarize.sh dist/Serialist-<version>-macos-arm64.dmg
   spctl --assess --type open --context context:primary-signature -v dist/Serialist-*.dmg
   ```

   `spctl` should say `accepted` and `source=Notarized Developer ID`. A rejected submission
   makes `notarize.sh` print the notary service's log and fail.
6. **Add the repository secrets** (Settings, Secrets and variables, Actions, or
   `gh secret set NAME`); the comment above the signing steps in `release.yml` says the
   same:

   | Secret | Value |
   | --- | --- |
   | `MACOS_CERTIFICATE` | the `.p12`, base64: `base64 -i cert.p12 \| gh secret set MACOS_CERTIFICATE` |
   | `MACOS_CERTIFICATE_PASSWORD` | the `.p12`'s export password |
   | `MACOS_KEYCHAIN_PASSWORD` | any new random string (it locks a keychain that lives for one job) |
   | `MACOS_SIGN_IDENTITY` | the identity from step 2, exactly |
   | `APPLE_ID` | the Apple ID's email |
   | `APPLE_TEAM_ID` | the team ID from step 2 |
   | `APPLE_APP_PASSWORD` | the app-specific password from step 4 |

7. **Uncomment the three marked pieces** of the `package` job: "Import the signing
   certificate", the `env` with `CODESIGN_IDENTITY` on "Build and package", and "Notarize
   and staple the disk image". With `CODESIGN_IDENTITY` set, `bundle.sh` signs the app with
   the identity, the hardened runtime (`--options runtime`) and a secure timestamp, and
   `dmg.sh` signs the `.dmg`; `notarize.sh` submits the `.dmg`, waits for the verdict and
   staples the ticket to it. Every macOS row does this, the Intel one included.
8. **Run the workflow by hand** on `main` (artifacts only) and check the macOS log: the
   imported identity listed by `find-identity`, `bundle.sh` printing
   `flags=0x10000(runtime)` and a `Timestamp=` line, and `notarize.sh` ending with "The
   validate action worked!". Open the downloaded `.dmg` on another Mac: it should open with
   no Gatekeeper warning.
9. **Remove the "unsigned" wording** about macOS from `README.md`,
   `packaging/release-notes-install.md`, [Unsigned builds](#unsigned-builds) and the header
   of `release.yml`.

The app is signed with no entitlements: it generates no code at run time (Lua 5.4 is
interpreted) and needs none for serial ports outside the App Sandbox. A build with the
`wasm` feature would need `com.apple.security.cs.allow-jit` (see `bundle.sh`). None of the
signing or notarization path has been run: it needs the credentials.

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
the MSVC linker. The same file holds the version resource that Explorer's Details tab
shows: file and product version from the package version (a pre-release is flagged as
one), product name, description, company and copyright as `main.wxs` and `Info.plist.in`
spell them, and the original file name. The bytes come from
`crates/serialist/build/windows_res.rs`, which `crates/serialist/tests/windows_res.rs` also
includes, so `cargo test` checks the layout on every platform, and checks that the strings
still match the other two packages. It does nothing on other targets.

## Known gaps

- **The macOS Intel and Linux arm64 rows have never run.** They were added without a run
  (nothing here can start the workflow). On the first manual run, check that: both jobs
  start at all (`macos-15-intel` and `ubuntu-22.04-arm` are GitHub-hosted labels, and the
  arm one is free only for public repositories); the Intel log says `packaging serialist
  <version> for macos-x86_64` and `otool -l target/release/serialist` on the binary
  (or `vtool -show-build`) reports `minos 11.0` (the workflow sets
  `MACOSX_DEPLOYMENT_TARGET` for it); the arm64 log says `for linux-aarch64` and
  `dpkg-deb --info` prints `Architecture: arm64`; the `draft release` job lists all seven
  files and `SHA256SUMS` has a line for each; and, with `draft_release` ticked, an extra row
  made to fail still leaves a draft with the four required packages. Then open the Intel
  `.dmg` on an Intel Mac and the arm64 `.deb` on an arm64 Linux desktop (a Raspberry Pi 5
  with a Vulkan driver, or an arm64 VM). Once both have passed a few releases, drop
  `extra: true` and add their patterns to the check in the `release` job.
- **Windows on ARM is not built.** A `windows-11-arm` row is not added, for three reasons
  that nothing here can settle. The dependency tree has `ring` (through the HTTP client),
  which needs `clang` to build for `aarch64-pc-windows-msvc`, and nothing shows that image
  has it on `PATH`. The workflow relies on the image for `rustup`, WiX Toolset 3.14 and
  Chocolatey (the WiX fallback), and none of them is confirmed there. And cargo-wix 0.3.9
  must pass `-arch arm64` to WiX for an ARM64 `.msi`; `main.wxs` already picks the 64-bit
  Program Files folder for `arm64`, but the pairing has not been built. Windows 11 on ARM
  runs the x86_64 `.msi` under emulation, which is the answer until then (also untried).
- **The Linux glibc floor** is that of the build runner (`ubuntu-22.04`, glibc 2.35). The
  tarball and `.deb` link the host's shared libraries: the `.deb` depends on what
  `dpkg-shlibdeps` finds plus `libvulkan1` (loaded at run time); a tarball user installs the
  libraries named in the README.
- **Linux window icon, untried on a desktop.** A desktop finds a window's icon and name
  through its `.desktop` entry: on Wayland it matches the window's app id against the
  entry's file name (`serialist` for `serialist.desktop`), on X11 the window's `WM_CLASS`
  against `StartupWMClass`. GPUI sends the main window's `app_id` as the Wayland app id
  and writes it as both halves of `WM_CLASS` on X11, so the app id is `serialist`
  (`open_main_window` in `crates/serialist-ui/src/workspace.rs`), the file installed by the
  `.deb` and the tarball is `serialist.desktop`, and the entry says
  `StartupWMClass=serialist`. Keep the three equal. Nobody has yet looked at the taskbar
  of a GNOME or KDE session, on Wayland or X11, with an installed package.
- **The Windows version resource has not been seen.** `serialist.exe` now carries a
  `VS_VERSIONINFO` (see [Changing the icon](#changing-the-icon)), and its bytes are tested
  against Microsoft's documented layout, but it was written on a Mac: no MSVC link has
  taken it yet, and nobody has opened Properties, Details on the result. The next Windows
  job of the release workflow links it; check the Details tab of the installed
  `serialist.exe`.
- **Linux and Windows packages are built but not tried.** The workflow has run three times
  (a manual run on `main` on 2026-09-30, then the `v0.1.0` and `v0.2.0` tags), and every
  job passed on all three systems: each built the release binary, ran `--version` on its
  runner and packaged it, and the two tag runs attached the `.dmg`, the `.tar.gz`, the
  `.deb`, the `.msi` and `SHA256SUMS` to the release. That proves the Linux and Windows
  binaries start far enough to print their version, not that they open a window. GitHub
  counts no download of the Linux or Windows files of either release, so nothing shows that
  the `.deb` or the tarball has been installed on a Linux desktop or the `.msi` on Windows.
  Step 6 of [Cutting a release](#cutting-a-release) is the first time that happens.
- **WiX warns about the Start menu shortcut.** Light reports ICE69 for `main.wxs`: the
  shortcut's component refers to `serialist.exe`, which belongs to another component of the
  same feature. The warning does not fail the build, and within one feature the reference
  holds; nobody has yet seen the shortcut on an installed copy.
