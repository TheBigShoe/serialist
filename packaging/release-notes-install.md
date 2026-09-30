## Installing

These builds are **not code-signed**, so every OS warns on first launch. `SHA256SUMS` lists the checksums.

- **macOS (Apple Silicon, macOS 11 or later):** open the `.dmg` and drag Serialist to Applications. Gatekeeper refuses an app that is not notarized. On macOS 14 and earlier, Control-click Serialist, choose Open and confirm. On macOS 15 and later, try to open it once, then open System Settings, Privacy & Security, and press Open Anyway next to the Serialist notice. Or clear the download flag: `xattr -dr com.apple.quarantine /Applications/Serialist.app`.
- **Windows (x86_64):** run the `.msi`. SmartScreen says "Windows protected your PC": choose More info, then Run anyway. The Customize page can add Serialist to `PATH`.
- **Linux (x86_64):** `sudo apt install ./serialist_*.deb`, or unpack the `.tar.gz` and run `./install.sh` (installs to `~/.local`). A Vulkan-capable GPU driver is required. Join the group that owns your serial devices (`dialout` on Debian and Ubuntu) to open ports.
