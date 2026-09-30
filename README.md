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

## License

Licensed under either of Apache License, Version 2.0 (`LICENSE-APACHE`) or MIT license
(`LICENSE-MIT`) at your option. Contributions are accepted under the same terms.
