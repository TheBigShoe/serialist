//! The example plugins bundled with the app. None of them decodes anything until the user
//! installs it: [`ConfigPaths::install_example_plugin`] copies its folder into the config
//! directory's `plugins/`, where the app loads it like any plugin of the user's own.
//!
//! | Folder | What it is |
//! |---|---|
//! | `airoha-race` | The Airoha RACE reference plugin in Lua, `assets/plugins/airoha-race/plugin.lua` |
//! | `airoha-race-wasm` | The same plugin as a WebAssembly component, built from `examples/plugins/airoha-race-wasm` (only with the `wasm` feature) |
//!
//! The WebAssembly one is the committed build under `tests/fixtures`, which
//! `tests/wasm_plugin.rs` checks against its source.
//!
//! [`ConfigPaths::install_example_plugin`]: serialist_core::settings::ConfigPaths::install_example_plugin

pub use serialist_core::settings::ExamplePlugin;

use crate::AIROHA_RACE_LUA;

/// Every example plugin this build can install, in the order menus list them.
pub const EXAMPLE_PLUGINS: &[ExamplePlugin] = &[
    ExamplePlugin {
        name: "airoha-race",
        title: "Airoha RACE",
        description: "Airoha RACE frames (0x05 sync, type, length, command id), in Lua",
        files: &[("plugin.lua", AIROHA_RACE_LUA.as_bytes())],
    },
    #[cfg(feature = "wasm")]
    ExamplePlugin {
        name: "airoha-race-wasm",
        title: "Airoha RACE (WebAssembly)",
        description: "The Airoha RACE plugin compiled to a WebAssembly component",
        files: &[
            (
                "plugin.toml",
                include_bytes!("../tests/fixtures/plugins/airoha-race-wasm/plugin.toml"),
            ),
            (
                "plugin.wasm",
                include_bytes!("../tests/fixtures/plugins/airoha-race-wasm/plugin.wasm"),
            ),
        ],
    },
];

/// The example plugin installed as `plugins/<name>/`, if this build bundles one.
pub fn example_plugin(name: &str) -> Option<&'static ExamplePlugin> {
    EXAMPLE_PLUGINS.iter().find(|example| example.name == name)
}
