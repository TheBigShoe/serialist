//! Turning a command and its parameter values into the bytes to send.
//!
//! # Text payloads
//!
//! `{ "text": "AT+ID={{id}}\\r" }` sends the text as UTF-8. Two things are special:
//!
//! - **Escapes**: `\r`, `\n`, `\t`, `\0`, `\\`, `\{`, `\}` and `\xNN` (two hex digits,
//!   one raw byte). Any other backslash sequence, and a trailing backslash, is an error.
//!   Control characters written directly in the JSON string pass through unchanged, so
//!   `"AT\r"` inside the file (one backslash to JSON, a real CR) works too.
//! - **Placeholders**: `{{name}}` is replaced by the parameter's value. The name must be
//!   one of the command's `params`; anything else is [`PayloadError::UnknownParam`].
//!   A value is checked against its kind and written in a canonical form: `text` as
//!   typed, `int` in decimal, `hex16` as four upper-case hex digits. Write `\{{` for a
//!   literal `{{`. A parameter's value is never expanded again, so it cannot smuggle in
//!   escapes or placeholders.
//!
//! # Hex payloads
//!
//! `{ "hex": "05 5A 02 00 {{id}}" }` is bytes as hex digits. Spaces, commas and newlines
//! separate them, `0x` prefixes are fine, and each run of digits needs an even count
//! (`0F`, not `F`). Placeholders are replaced before the digits are read:
//!
//! - a `hex16` value becomes two bytes, little-endian, or big-endian with
//!   `{{id:be}}` (`{{id:le}}` is the default spelled out);
//! - an `int` value becomes one byte, so it must be 0 to 255;
//! - a `text` value is spliced in as hex digits.
//!
//! # Line endings
//!
//! The command's own `eol` wins. Without one, a text payload gets the session's line
//! ending and a hex or codec payload gets none.

use std::collections::BTreeMap;

use serde_json::Value;

use super::model::{Command, Param, ParamKind, Payload};
use crate::settings::LineEnding;

/// Values for a command's parameters, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParamValues(BTreeMap<String, String>);

impl ParamValues {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `name`, replacing an earlier value.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.0.insert(name.into(), value.into());
        self
    }

    /// [`set`](Self::set) for building a value list in one expression.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set(name, value);
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for ParamValues {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

/// Why a command could not be turned into bytes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PayloadError {
    /// A placeholder's parameter has no value and no default.
    #[error("no value for parameter `{0}`, and it has no default")]
    MissingParam(String),
    /// A `{{name}}` that is not one of the command's `params`.
    #[error("`{{{{{0}}}}}` does not name a parameter of this command")]
    UnknownParam(String),
    /// A value that does not fit its parameter's kind.
    #[error("parameter `{name}` is `{value}`: {reason}")]
    BadParamValue {
        name: String,
        value: String,
        reason: String,
    },
    /// A backslash sequence that is not an escape. Holds what follows the backslash.
    #[error("unknown escape `\\{0}`")]
    BadEscape(String),
    /// A `{{` with no `}}` after it.
    #[error("`{{{{` is never closed by `}}}}`")]
    UnterminatedPlaceholder,
    /// The text between `{{` and `}}` is not a parameter name with an optional modifier.
    #[error("`{{{{{0}}}}}` is not a valid placeholder")]
    BadPlaceholder(String),
    /// A modifier that does not apply: only `:le` and `:be`, on a `hex16` parameter in a
    /// hex payload.
    #[error("`{{{{{name}:{modifier}}}}}`: {reason}")]
    BadModifier {
        name: String,
        modifier: String,
        reason: String,
    },
    /// Something in a hex payload that is not bytes.
    #[error("bad hex `{token}`: {reason}")]
    BadHex { token: String, reason: String },
    /// A codec payload: the app encodes it with the codec it names (see
    /// `serialist_plugins::encode_payload`), so this crate has no bytes for it.
    #[error("codec payloads are encoded by the codec they name, which is not available here")]
    CodecUnavailable,
    /// A `{ "script": … }` payload: sending the command runs the script, so it has no
    /// bytes. Holds the script's path as written.
    #[error("this command runs the script {0}; it has no bytes to send")]
    ScriptPayload(String),
}

impl Command {
    /// The bytes to send: the payload with placeholders filled from `params` (or the
    /// parameter's default), then the line ending. That is the command's `eol` if it has
    /// one, else `session_eol` for a text payload and nothing for hex and codec ones.
    /// See the [module docs](self) for the payload syntax.
    ///
    /// A codec payload always fails with [`PayloadError::CodecUnavailable`], and a
    /// script payload with [`PayloadError::ScriptPayload`]: the app runs it instead.
    pub fn encode(
        &self,
        params: &ParamValues,
        session_eol: LineEnding,
    ) -> Result<Vec<u8>, PayloadError> {
        let (mut bytes, default_eol) = match &self.payload {
            Payload::Text(text) => (self.expand_text(text, params)?, session_eol),
            Payload::Hex(hex) => (self.expand_hex(hex, params)?, LineEnding::None),
            Payload::Codec { .. } => return Err(PayloadError::CodecUnavailable),
            Payload::Script { path } => {
                return Err(PayloadError::ScriptPayload(path.display().to_string()));
            }
        };
        bytes.extend_from_slice(self.eol.unwrap_or(default_eol).bytes());
        Ok(bytes)
    }

    /// The names of the parameters the payload uses, without repeats, in order of first
    /// use. A payload with broken syntax gives the names found before the break.
    pub fn placeholders(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut scan = |template: &str, escapes: bool| {
            let mut tokens = Vec::new();
            // A syntax error is `encode`'s to report; the names before it still count.
            let _ = tokenize(template, escapes, &mut tokens);
            for token in tokens {
                if let Token::Param { name, .. } = token
                    && !names.contains(&name)
                {
                    names.push(name);
                }
            }
        };
        match &self.payload {
            Payload::Text(text) => scan(text, true),
            Payload::Hex(hex) => scan(hex, false),
            Payload::Codec { fields, .. } => {
                let mut strings = Vec::new();
                collect_strings(fields.values(), &mut strings);
                for text in strings {
                    scan(text, false);
                }
            }
            Payload::Script { .. } => {}
        }
        names
    }

    /// The prefilled value of every parameter that has a default, for a prompt to start
    /// from.
    pub fn defaults(&self) -> ParamValues {
        self.params
            .iter()
            .filter_map(|param| Some((param.name.clone(), param.default.clone()?)))
            .collect()
    }

    /// Everything wrong with this command that does not depend on the values a user will
    /// type, as messages for a warning list: bad parameters, an undeclared placeholder,
    /// broken escapes or hex, a pattern that is not a regex. Empty means it should send.
    pub fn problems(&self) -> Vec<String> {
        let mut found = Vec::new();
        if self.name.trim().is_empty() {
            found.push("the command has no name".to_owned());
        }
        for (index, param) in self.params.iter().enumerate() {
            if !valid_name(&param.name) {
                found.push(format!(
                    "`{}` is not a parameter name (letters, digits, `_` and `-`)",
                    param.name
                ));
            } else if self.params[..index].iter().any(|p| p.name == param.name) {
                found.push(format!("parameter `{}` is declared twice", param.name));
            }
            if let Some(default) = &param.default
                && let Err(err) = param.validate(default)
            {
                found.push(format!("default of `{}`: {err}", param.name));
            }
        }
        // Encode once with each parameter at its default (or a stand-in), which finds
        // bad escapes, bad hex, undeclared placeholders and bad modifiers.
        let sample: ParamValues = self
            .params
            .iter()
            .map(|param| {
                let value = param.default.clone().unwrap_or_else(|| match param.kind {
                    ParamKind::Text => String::new(),
                    ParamKind::Int => "0".to_owned(),
                    ParamKind::Hex16 => "0000".to_owned(),
                });
                (param.name.clone(), value)
            })
            .collect();
        match self.encode(&sample, LineEnding::None) {
            Ok(_) | Err(PayloadError::CodecUnavailable | PayloadError::ScriptPayload(_)) => {}
            Err(err) => found.push(err.to_string()),
        }
        if let Some(expect) = &self.expect {
            match &expect.frame {
                Some(frame) if frame.is_empty() => {
                    found.push("expect frame names nothing to match".to_owned());
                }
                Some(_) => {}
                None if expect.pattern.is_empty() => {
                    found.push("expect needs a pattern or a frame".to_owned());
                }
                None => {}
            }
            if !expect.pattern.is_empty()
                && let Err(err) = crate::matcher::compile_pattern(&expect.pattern)
            {
                found.push(format!("expect pattern: {err}"));
            }
            if expect.timeout_ms == 0 {
                found.push("expect timeout_ms is 0, so it always times out".to_owned());
            }
        }
        if let Some(keys) = &self.keybinding
            && keys.split_whitespace().next().is_none()
        {
            found.push("the keybinding is empty".to_owned());
        }
        found
    }

    fn expand_text(&self, text: &str, params: &ParamValues) -> Result<Vec<u8>, PayloadError> {
        let mut tokens = Vec::new();
        tokenize(text, true, &mut tokens)?;
        let mut out = Vec::new();
        for token in tokens {
            match token {
                Token::Lit(bytes) => out.extend_from_slice(&bytes),
                Token::Param { name, modifier } => {
                    if let Some(modifier) = modifier {
                        return Err(PayloadError::BadModifier {
                            name,
                            modifier,
                            reason: "modifiers only apply in hex payloads".to_owned(),
                        });
                    }
                    let (param, value) = self.resolve(&name, params)?;
                    out.extend_from_slice(param.text_form(value)?.as_bytes());
                }
            }
        }
        Ok(out)
    }

    fn expand_hex(&self, hex: &str, params: &ParamValues) -> Result<Vec<u8>, PayloadError> {
        let mut tokens = Vec::new();
        tokenize(hex, false, &mut tokens)?;
        let mut digits = String::new();
        for token in tokens {
            match token {
                Token::Lit(bytes) => digits.push_str(&String::from_utf8_lossy(&bytes)),
                Token::Param { name, modifier } => {
                    let (param, value) = self.resolve(&name, params)?;
                    digits.push_str(&param.hex_form(value, modifier.as_deref())?);
                }
            }
        }
        parse_hex(&digits)
    }

    /// The declared parameter and the value to use for it.
    fn resolve<'a>(
        &'a self,
        name: &str,
        params: &'a ParamValues,
    ) -> Result<(&'a Param, &'a str), PayloadError> {
        let param = self
            .params
            .iter()
            .find(|param| param.name == name)
            .ok_or_else(|| PayloadError::UnknownParam(name.to_owned()))?;
        let value = params
            .get(name)
            .or(param.default.as_deref())
            .ok_or_else(|| PayloadError::MissingParam(name.to_owned()))?;
        Ok((param, value))
    }
}

impl Param {
    /// Checks `value` against this parameter's kind, for a prompt to show as the user
    /// types.
    pub fn validate(&self, value: &str) -> Result<(), PayloadError> {
        match self.kind {
            ParamKind::Text => Ok(()),
            ParamKind::Int => parse_int(value).map(drop),
            ParamKind::Hex16 => parse_hex16(value).map(drop),
        }
        .map_err(|reason| self.bad_value(value, reason))
    }

    fn bad_value(&self, value: &str, reason: String) -> PayloadError {
        PayloadError::BadParamValue {
            name: self.name.clone(),
            value: value.to_owned(),
            reason,
        }
    }

    /// The value as it goes into a text payload.
    fn text_form(&self, value: &str) -> Result<String, PayloadError> {
        Ok(match self.kind {
            ParamKind::Text => value.to_owned(),
            ParamKind::Int => parse_int(value)
                .map_err(|reason| self.bad_value(value, reason))?
                .to_string(),
            ParamKind::Hex16 => format!(
                "{:04X}",
                parse_hex16(value).map_err(|reason| self.bad_value(value, reason))?
            ),
        })
    }

    /// The value as hex digits in a hex payload.
    fn hex_form(&self, value: &str, modifier: Option<&str>) -> Result<String, PayloadError> {
        let bad_modifier = |modifier: &str, reason: &str| PayloadError::BadModifier {
            name: self.name.clone(),
            modifier: modifier.to_owned(),
            reason: reason.to_owned(),
        };
        if let Some(modifier) = modifier
            && self.kind != ParamKind::Hex16
        {
            return Err(bad_modifier(
                modifier,
                "`:le` and `:be` only apply to hex16 parameters",
            ));
        }
        match self.kind {
            ParamKind::Text => Ok(value.to_owned()),
            ParamKind::Int => {
                let number = parse_int(value).map_err(|reason| self.bad_value(value, reason))?;
                u8::try_from(number)
                    .map(|byte| format!("{byte:02X}"))
                    .map_err(|_| {
                        self.bad_value(
                            value,
                            "does not fit in one byte (0 to 255); use a hex16 parameter for two"
                                .to_owned(),
                        )
                    })
            }
            ParamKind::Hex16 => {
                let number = parse_hex16(value).map_err(|reason| self.bad_value(value, reason))?;
                let [high, low] = number.to_be_bytes();
                match modifier {
                    None => Ok(format!("{low:02X} {high:02X}")),
                    Some(m) if m.eq_ignore_ascii_case("le") => Ok(format!("{low:02X} {high:02X}")),
                    Some(m) if m.eq_ignore_ascii_case("be") => Ok(format!("{high:02X} {low:02X}")),
                    Some(m) => Err(bad_modifier(m, "the modifiers are `:le` and `:be`")),
                }
            }
        }
    }
}

/// A parameter name: letters, digits, `_` and `-`.
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

fn parse_int(value: &str) -> Result<i64, String> {
    const NEEDED: &str = "not a whole number (decimal, or hex with 0x)";
    let text = value.trim();
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    // A second sign, or none of the digits, is not a number.
    if !unsigned.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(NEEDED.to_owned());
    }
    let magnitude = match unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        Some(hex) if hex.chars().all(|c| c.is_ascii_hexdigit()) => i64::from_str_radix(hex, 16),
        Some(_) => return Err(NEEDED.to_owned()),
        None => unsigned.parse::<i64>(),
    };
    magnitude
        .map(|n| if negative { -n } else { n })
        .map_err(|_| NEEDED.to_owned())
}

fn parse_hex16(value: &str) -> Result<u16, String> {
    let text = value.trim();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("needs 1 to 4 hex digits, such as 0F15 or 0x0F15".to_owned());
    }
    u16::from_str_radix(digits, 16).map_err(|err| err.to_string())
}

/// Reads hex digits: runs separated by whitespace or commas, each with an optional `0x`
/// and an even digit count.
fn parse_hex(text: &str) -> Result<Vec<u8>, PayloadError> {
    let mut out = Vec::new();
    for token in text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|token| !token.is_empty())
    {
        let bad = |reason: String| PayloadError::BadHex {
            token: token.to_owned(),
            reason,
        };
        let digits = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"))
            .unwrap_or(token);
        if digits.is_empty() {
            return Err(bad("no digits after 0x".to_owned()));
        }
        if let Some(c) = digits.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(bad(format!("`{c}` is not a hex digit")));
        }
        if digits.len() % 2 != 0 {
            return Err(bad(
                "an odd number of digits; write each byte as two (0F, not F)".to_owned(),
            ));
        }
        for pair in digits.as_bytes().chunks(2) {
            // Both digits were checked above.
            let byte = pair.iter().fold(0u8, |acc, d| {
                (acc << 4) | (*d as char).to_digit(16).unwrap_or(0) as u8
            });
            out.push(byte);
        }
    }
    Ok(out)
}

/// A piece of a payload template.
enum Token {
    /// Bytes to send as they are, escapes already applied.
    Lit(Vec<u8>),
    Param {
        name: String,
        modifier: Option<String>,
    },
}

/// Splits `template` into literal bytes and placeholders, applying escapes when
/// `escapes` is set. Whatever was read before an error is left in `out`.
fn tokenize(template: &str, escapes: bool, out: &mut Vec<Token>) -> Result<(), PayloadError> {
    fn flush(lit: &mut Vec<u8>, out: &mut Vec<Token>) {
        if !lit.is_empty() {
            out.push(Token::Lit(std::mem::take(lit)));
        }
    }
    let bytes = template.as_bytes();
    let mut lit = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if escapes => {
                let next = template[i + 1..].chars().next();
                let simple = match next {
                    Some('r') => Some(b'\r'),
                    Some('n') => Some(b'\n'),
                    Some('t') => Some(b'\t'),
                    Some('0') => Some(0),
                    Some('\\') => Some(b'\\'),
                    Some('{') => Some(b'{'),
                    Some('}') => Some(b'}'),
                    _ => None,
                };
                if let Some(byte) = simple {
                    lit.push(byte);
                    i += 2;
                } else if next == Some('x') {
                    let digits = template
                        .get(i + 2..i + 4)
                        .filter(|d| d.chars().all(|c| c.is_ascii_hexdigit()));
                    let Some(digits) = digits else {
                        let seen: String = template[i + 1..].chars().take(3).collect();
                        flush(&mut lit, out);
                        return Err(PayloadError::BadEscape(seen));
                    };
                    lit.push(u8::from_str_radix(digits, 16).unwrap_or(0));
                    i += 4;
                } else {
                    flush(&mut lit, out);
                    return Err(PayloadError::BadEscape(
                        next.map(String::from).unwrap_or_default(),
                    ));
                }
            }
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                flush(&mut lit, out);
                let Some(len) = template[i + 2..].find("}}") else {
                    return Err(PayloadError::UnterminatedPlaceholder);
                };
                let inner = &template[i + 2..i + 2 + len];
                let (name, modifier) = match inner.split_once(':') {
                    Some((name, modifier)) => (name.trim(), Some(modifier.trim())),
                    None => (inner.trim(), None),
                };
                if !valid_name(name) || modifier == Some("") {
                    return Err(PayloadError::BadPlaceholder(inner.trim().to_owned()));
                }
                out.push(Token::Param {
                    name: name.to_owned(),
                    modifier: modifier.map(str::to_owned),
                });
                i += 2 + len + 2;
            }
            byte => {
                lit.push(byte);
                i += 1;
            }
        }
    }
    flush(&mut lit, out);
    Ok(())
}

/// Every string inside `values`, however deep.
fn collect_strings<'a>(values: impl Iterator<Item = &'a Value>, out: &mut Vec<&'a str>) {
    for value in values {
        match value {
            Value::String(text) => out.push(text),
            Value::Array(items) => collect_strings(items.iter(), out),
            Value::Object(map) => collect_strings(map.values(), out),
            _ => {}
        }
    }
}
