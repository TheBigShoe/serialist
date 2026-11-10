## Installing

These builds are **not code-signed**, so every OS warns on first launch. `SHA256SUMS` lists the checksums.

- **macOS (macOS 11 or later):** `-macos-arm64.dmg` is for Apple Silicon, `-macos-x86_64.dmg` for Intel Macs. Open the `.dmg` and drag Serialist to Applications. Gatekeeper refuses an app that is not notarized. On macOS 14 and earlier, Control-click Serialist, choose Open and confirm. On macOS 15 and later, try to open it once, then open System Settings, Privacy & Security, and press Open Anyway next to the Serialist notice. Or clear the download flag: `xattr -dr com.apple.quarantine /Applications/Serialist.app`.
- **Windows (x86_64):** run the `.msi`. SmartScreen says "Windows protected your PC": choose More info, then Run anyway. The Customize page can add Serialist to `PATH`.
- **Linux (x86_64 and arm64):** the `amd64` `.deb` and `x86_64` `.tar.gz` are for Intel and AMD machines, the `arm64` `.deb` and `aarch64` `.tar.gz` for 64-bit ARM. `sudo apt install ./serialist_*_amd64.deb` (or `_arm64.deb`), or unpack the `.tar.gz` and run `./install.sh` (installs to `~/.local`). A Vulkan-capable GPU driver is required. Join the group that owns your serial devices (`dialout` on Debian and Ubuntu) to open ports.
