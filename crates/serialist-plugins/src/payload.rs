//! Encoding a saved command's codec payload, `{ "codec": …, "fields": { … } }`.

use serde_json::{Map, Value as JsonValue};
use serialist_core::codec::{CodecError, CodecRegistry, EncodeRequest};

/// Why a codec payload could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PayloadError {
    /// No codec by this name is registered.
    #[error("no codec named `{0}` is loaded")]
    UnknownCodec(String),
    /// The fields name no command and the codec has none to default to.
    #[error("codec `{0}` has no commands to encode")]
    NoCommands(String),
    /// The codec refused the command, or could not be started.
    #[error("codec `{codec}`: {source}")]
    Codec {
        codec: String,
        #[source]
        source: CodecError,
    },
}

/// Something that turns a codec payload into bytes. The command sender takes one, so a
/// saved command's codec payload needs no knowledge of which codecs exist.
pub trait PayloadEncoder {
    /// The bytes for `fields` (a saved command's `fields` object) encoded by the codec
    /// called `codec`. A `command` key picks the codec's command; without one it is the
    /// codec's first ([`CodecInfo::default_command`](serialist_core::CodecInfo::default_command)).
    fn encode_payload(
        &self,
        codec: &str,
        fields: &Map<String, JsonValue>,
    ) -> Result<Vec<u8>, PayloadError>;
}

impl PayloadEncoder for CodecRegistry {
    fn encode_payload(
        &self,
        codec: &str,
        fields: &Map<String, JsonValue>,
    ) -> Result<Vec<u8>, PayloadError> {
        encode_payload(self, codec, fields)
    }
}

/// [`PayloadEncoder::encode_payload`] over a registry. Makes a fresh codec instance for
/// the call, so it never disturbs a session's decoding state.
pub fn encode_payload(
    registry: &CodecRegistry,
    codec: &str,
    fields: &Map<String, JsonValue>,
) -> Result<Vec<u8>, PayloadError> {
    let factory = registry
        .get(codec)
        .ok_or_else(|| PayloadError::UnknownCodec(codec.to_owned()))?;
    let wrap = |source| PayloadError::Codec {
        codec: codec.to_owned(),
        source,
    };
    let info = factory.info();
    let default = match info.default_command() {
        Some(command) => command.name.as_str(),
        None if fields.contains_key("command") => "",
        None => return Err(PayloadError::NoCommands(codec.to_owned())),
    };
    let request = EncodeRequest::from_payload(fields, default).map_err(wrap)?;
    factory
        .create()
        .map_err(wrap)?
        .encode(&request)
        .map_err(wrap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(value: JsonValue) -> Map<String, JsonValue> {
        match value {
            JsonValue::Object(map) => map,
            _ => panic!("an object"),
        }
    }

    #[test]
    fn saved_command_payloads_encode_through_the_registry() {
        let mut registry = CodecRegistry::new();
        registry.register(crate::AirohaRace::factory());
        registry.register(crate::TextLines::factory());
        // The command defaults to the codec's first, `race`.
        assert_eq!(
            registry.encode_payload("airoha-race", &fields(json!({ "cmd_id": "0x0F15" }))),
            Ok(vec![0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F])
        );
        assert_eq!(
            encode_payload(
                &registry,
                "airoha-race",
                &fields(json!({ "command": "race_version" }))
            ),
            Ok(vec![0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F])
        );
        assert_eq!(
            encode_payload(
                &registry,
                "text-lines",
                &fields(json!({ "text": "AT", "eol": "cr" }))
            ),
            Ok(b"AT\r".to_vec())
        );
        assert_eq!(
            encode_payload(&registry, "nope", &Map::new()),
            Err(PayloadError::UnknownCodec("nope".into()))
        );
        assert_eq!(
            encode_payload(&registry, "airoha-race", &fields(json!({ "command": "x" }))),
            Err(PayloadError::Codec {
                codec: "airoha-race".into(),
                source: CodecError::UnknownCommand("x".into()),
            })
        );
        assert_eq!(
            encode_payload(&registry, "airoha-race", &Map::new()),
            Err(PayloadError::Codec {
                codec: "airoha-race".into(),
                source: CodecError::MissingField("cmd_id".into()),
            })
        );
    }
}
