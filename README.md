# Serialist

A native, GPU-rendered serial terminal built on [GPUI](https://gpui.rs), the UI framework
behind the Zed editor. macOS first, Linux and Windows from the same code.

- Devices appear the moment they are plugged in.
- Any baud rate, full framing and flow control settings, per-device profiles.
- Two modes: inline interactive, and saved commands you can send with a click or a key.
- Lua scripting and protocol plugins (Airoha RACE ships as the first one).
- Fonts and themes configured the way Zed does it; Zed theme files load unchanged.
- Fast: no dropped bytes at 3 Mbaud, frames under 8 ms at a million lines of scrollback.
- Every test runs without hardware.

Status: milestone 0. See `docs/plan.md`.

## Installing a release

Release builds are attached to
[GitHub Releases](https://github.com/TheBigShoe/serialist/releases): a `.dmg` for macOS
(Apple Silicon), a `.tar.gz` and a `.deb` for Linux (x86_64), and an `.msi` for Windows
(x86_64). `SHA256SUMS` lists the checksums. **None of them are code-signed yet**, so each
OS warns on first launch:

- **macOS.** Open the `.dmg` and drag Serialist to Applications. The app is signed ad hoc
  but not with an Apple Developer ID and not notarized, so Gatekeeper refuses it at first
  ("Apple could not verify..."). Either Control-click Serialist in Applications, choose
  Open, and confirm (macOS 14 and earlier); or try to open it once, then go to System
  Settings, Privacy & Security, and press Open Anyway next to the Serialist notice
  (macOS 15 and later); or clear the download flag in Terminal:
  `xattr -dr com.apple.quarantine /Applications/Serialist.app`.
- **Windows.** SmartScreen shows "Windows protected your PC". Choose More info, then Run
  anyway. Installing from the `.msi` puts Serialist in Program Files and the Start menu;
  the Customize page has an option to add it to `PATH`.
- **Linux.** `sudo apt install ./serialist_*.deb`, or unpack the `.tar.gz` and run
  `./install.sh` (installs to `~/.local`). The `.deb` pulls in what it needs; for the
  tarball install the Vulkan loader and a Vulkan-capable GPU driver, xkbcommon
  (`libxkbcommon`, `libxkbcommon-x11`), fontconfig, libudev, and the Wayland or X11 client
  libraries (Debian and Ubuntu: `libvulkan1 libxkbcommon0 libxkbcommon-x11-0 libfontconfig1
  libudev1 libwayland-client0 libx11-xcb1`). To open serial ports, join the group that owns
  them (`dialout` on Debian and Ubuntu).

To build the packages yourself, see `docs/releasing.md`.

## License

Licensed under either of Apache License, Version 2.0 (`LICENSE-APACHE`) or MIT license
(`LICENSE-MIT`) at your option. Contributions are accepted under the same terms.
