//! `plugin.toml`, the manifest next to a `plugin.wasm`.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use super::API_VERSION;

/// File name of the manifest in a plugin folder.
pub const MANIFEST_FILE: &str = "plugin.toml";

/// What a WebAssembly plugin says about itself before it is run, as Zed's
/// `extension.toml` does:
///
/// ```toml
/// name = "airoha-race"   # must match describe().name
/// version = "1.0.0"      # must match describe().version
/// api = "1"              # the WIT the plugin was built against (wit/v1)
/// description = "Airoha RACE framing"
/// ```
///
/// Other keys are ignored, so a later manifest can add some without breaking this host.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    /// The plugin API version, `"1"`: which `wit/v<api>` world the component exports.
    pub api: String,
    #[serde(default)]
    pub description: String,
}

impl PluginManifest {
    /// Parse and check a manifest's text: the name must not be empty and the API must be
    /// one this host speaks.
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: PluginManifest = toml::from_str(text).map_err(|err| err.to_string())?;
        if manifest.name.trim().is_empty() {
            return Err("`name` is empty".to_owned());
        }
        if manifest.api != API_VERSION {
            return Err(format!(
                "`api = {:?}` is not a plugin API this build of Serialist speaks; it loads \
                 `api = {API_VERSION:?}` plugins (rebuild the plugin against wit/v{API_VERSION})",
                manifest.api
            ));
        }
        Ok(manifest)
    }

    /// The manifest in plugin folder `dir`.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        Self::parse(&text).map_err(|err| format!("{}: {err}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_names_the_plugin_and_pins_the_api() {
        let manifest = PluginManifest::parse(
            "name = \"x\"\nversion = \"1.0.0\"\napi = \"1\"\nfuture = true\n",
        )
        .unwrap();
        assert_eq!(manifest.name, "x");
        assert_eq!(manifest.description, "");
    }

    #[test]
    fn bad_manifests_say_what_is_wrong() {
        let cases = [
            ("name = \"x\"\nversion = \"1\"\napi = \"2\"", "api = \"2\""),
            ("name = \"x\"\nversion = \"1\"\napi = 1", "invalid type"),
            ("name = \"x\"\nversion = \"1\"", "missing field `api`"),
            (
                "name = \" \"\nversion = \"1\"\napi = \"1\"",
                "`name` is empty",
            ),
            ("name = ", "TOML parse error"),
        ];
        for (text, hint) in cases {
            let err = PluginManifest::parse(text).unwrap_err();
            assert!(err.contains(hint), "{text:?}: {err}");
        }
    }
}
