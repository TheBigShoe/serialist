//! The Lua adapter: the plugin contract, the sandbox, the limits, reload and discovery.

mod common;

use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use serialist_core::{Codec, CodecError, CodecRegistry, EncodeRequest, Frame, Severity, Value};
use serialist_plugins::{
    LuaCodec, LuaLimits, PLUGIN_ERROR_KIND, PluginKind, bundled_race_lua, find_plugins,
    load_plugins,
};

use common::{TempDir, decode_chunks};

/// A line framer: one `line` frame per LF-terminated line, the rest held back.
const LINES: &str = r##"
local M = {}

function M.describe()
  return {
    name = "lines", version = "0.1.0", description = "test",
    kinds = {
      { kind = "line", fields = {
          { name = "text", type = "str" },
          { name = "n", type = "uint" },
          { name = "body", type = "bytes", optional = true },
      } },
    },
    commands = { { name = "say", fields = { { name = "text", type = "str" } } } },
  }
end

function M.decode(bytes, state)
  state.calls = (state.calls or 0) + 1
  local frames, i = {}, 1
  while true do
    local j = bytes:find("\n", i, true)
    if not j then break end
    local text = bytes:sub(i, j - 1)
    if text == "loop" then while true do end end
    if text == "swallow" then pcall(function() while true do end end) end
    if text == "grow" then
      local t = {}
      for k = 1, 1e9 do t[k] = string.rep("x", 4096) .. k end
    end
    if text == "boom" then error("boom") end
    local frame = {
      kind = "line", pos = i, len = j - i + 1, summary = text,
      fields = { text = text, n = #text, zeta = true, alpha = 1.5, calls = state.calls },
    }
    if text == "badtype" then frame.fields.n = "seven" end
    if text == "missing" then frame.fields.n = nil end
    if text == "outside" then frame.pos = #bytes + 5 end
    if text == "warn" then frame.severity = "warning" end
    frames[#frames + 1] = frame
    i = j + 1
  end
  if bytes:find("#", 1, true) then return frames, "not a suffix" end
  return frames, bytes:sub(i)
end

function M.encode(request)
  local f = request.fields
  if request.command == "say" then
    if f.text == nil then return nil, codec.missing_field("text") end
    if type(f.text) ~= "string" then return nil, codec.bad_field("text", "must be a string") end
    return f.text .. "\n"
  elseif request.command == "table" then
    return { 1, 2, 255 }
  elseif request.command == "types" then
    local ok = f.a == codec.null and codec.is_array(f.b) and not codec.is_array(f.c)
      and f.c.d == 1 and math.type(f.e) == "float" and math.type(f.f) == "integer"
      and f.b[2] == 2 and codec.is_array(f.g) and #f.g == 0
    return ok and "typed" or "untyped"
  elseif request.command == "fail" then
    return nil, "plain message"
  elseif request.command == "raise" then
    error("raised here")
  end
  return nil, codec.unknown_command(request.command)
end

return M
"##;

fn limits() -> LuaLimits {
    LuaLimits {
        memory_bytes: 16 * 1024 * 1024,
        instructions_per_call: 2_000_000,
        ..LuaLimits::default()
    }
}

fn lines() -> LuaCodec {
    LuaCodec::from_source("lines.lua", LINES, limits()).expect("the test plugin loads")
}

fn texts(frames: &[Frame]) -> Vec<(&str, std::ops::Range<u64>)> {
    frames
        .iter()
        .map(|f| (f.kind.as_str(), f.raw.clone()))
        .collect()
}

#[test]
fn held_back_bytes_are_prepended_and_offsets_stay_global() {
    let mut codec = lines();
    let frames = decode_chunks(&mut codec, &[b"ab", b"c\nde", b"f\ng"], Instant::now());
    assert_eq!(texts(&frames), [("line", 0..4), ("line", 4..8)]);
    assert_eq!(codec.held_back(), 1);
    let first = &frames[0];
    assert_eq!(first.summary, "abc");
    // Declared fields in order and type, then undeclared ones sorted, types inferred.
    assert_eq!(
        first.fields,
        [
            ("text".into(), Value::Str("abc".into())),
            ("n".into(), Value::UInt(3)),
            ("alpha".into(), Value::Float(1.5)),
            ("calls".into(), Value::Int(2)),
            ("zeta".into(), Value::Bool(true)),
        ]
    );
    assert_eq!(first.severity, Severity::Info);
}

#[test]
fn a_runaway_decode_becomes_a_plugin_error_and_decoding_goes_on() {
    for trap in ["loop", "swallow", "boom", "grow"] {
        let mut codec = lines();
        let started = Instant::now();
        let input = format!("ok\n{trap}\n");
        let frames = decode_chunks(&mut codec, &[input.as_bytes(), b"after\n"], started);
        assert_eq!(
            texts(&frames),
            [
                (PLUGIN_ERROR_KIND, 0..input.len() as u64),
                ("line", input.len() as u64..input.len() as u64 + 6)
            ],
            "{trap}"
        );
        let error = &frames[0];
        assert_eq!(error.severity, Severity::Error);
        let message = error.field("error").and_then(Value::as_str).unwrap();
        let expected = match trap {
            "loop" | "swallow" => "budget of 2000000 instructions",
            "boom" => "boom",
            _ => "memory error",
        };
        assert!(message.contains(expected), "{trap}: {message}");
        // The state starts over after an error.
        assert_eq!(frames[1].field("calls"), Some(&Value::Int(1)), "{trap}");
        assert!(started.elapsed().as_secs() < 10, "{trap} stalled");
    }
}

#[test]
fn frames_that_break_the_contract_are_reported_not_trusted() {
    let mut codec = lines();
    let frames = decode_chunks(
        &mut codec,
        &[b"badtype\nmissing\noutside\nwarn\n"],
        Instant::now(),
    );
    let got: Vec<_> = frames
        .iter()
        .map(|f| {
            (
                f.kind.as_str(),
                f.raw.clone(),
                f.field("error")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect();
    assert_eq!(got[0].0, PLUGIN_ERROR_KIND);
    assert_eq!(got[0].1, 0..8, "a bad frame's error covers that frame");
    assert!(
        got[0]
            .2
            .contains("field `n` is a string but is declared uint"),
        "{got:?}"
    );
    assert_eq!(got[1].1, 8..16);
    assert!(
        got[1].2.contains("field `n` is declared but missing"),
        "{got:?}"
    );
    // A frame outside the input spoils the rest of the call.
    assert_eq!(got[2].1, 0..29);
    assert!(got[2].2.contains("fall outside"), "{got:?}");
    assert_eq!(got.len(), 3);

    let mut codec = lines();
    let frames = decode_chunks(&mut codec, &[b"x\n#"], Instant::now());
    assert_eq!(texts(&frames), [("line", 0..2), (PLUGIN_ERROR_KIND, 0..3)]);
    assert_eq!(codec.held_back(), 0);

    let frames = decode_chunks(&mut lines(), &[b"warn\n"], Instant::now());
    assert_eq!(frames[0].severity, Severity::Warning);
}

#[test]
fn encode_maps_results_and_errors() {
    let mut codec = lines();
    let say = |fields| EncodeRequest {
        command: "say".into(),
        fields,
    };
    let fields = |v: serde_json::Value| v.as_object().cloned().unwrap();
    assert_eq!(
        codec.encode(&say(fields(json!({ "text": "hi" })))),
        Ok(b"hi\n".to_vec())
    );
    assert_eq!(
        codec.encode(&say(fields(json!({})))),
        Err(CodecError::MissingField("text".into()))
    );
    assert_eq!(
        codec.encode(&say(fields(json!({ "text": 1 })))),
        Err(CodecError::bad_field("text", "must be a string"))
    );
    assert_eq!(
        codec.encode(&EncodeRequest::new("table")),
        Ok(vec![1, 2, 255])
    );
    assert_eq!(
        codec.encode(&EncodeRequest::new("nope")),
        Err(CodecError::UnknownCommand("nope".into()))
    );
    assert_eq!(
        codec.encode(&EncodeRequest::new("fail")),
        Err(CodecError::Internal("plain message".into()))
    );
    assert!(matches!(
        codec.encode(&EncodeRequest::new("raise")),
        Err(CodecError::Internal(message)) if message.contains("raised here")
    ));
    let types = EncodeRequest {
        command: "types".into(),
        fields: fields(json!({
            "a": null, "b": [1, 2], "c": { "d": 1 }, "e": 1.5, "f": 2, "g": []
        })),
    };
    assert_eq!(codec.encode(&types), Ok(b"typed".to_vec()));
}

#[test]
fn the_sandbox_has_no_io_os_or_debug() {
    let plugin = r#"
      assert(io == nil and debug == nil and package == nil and require == nil)
      assert(dofile == nil and loadfile == nil and string.dump == nil)
      assert(os.execute == nil and os.getenv == nil and os.remove == nil and os.exit == nil)
      assert(type(os.time()) == "number" and type(os.clock()) == "number")
      assert(load("\27Lua", "bin", "b") == nil, "binary chunks are refused")
      assert(load("return 41 + 1")() == 42)
      assert(hex.encode("\5\90", "") == "055A" and hex.decode("05 5A") == "\5\90")
      assert(string.unpack("<I2", string.pack("<I2", 0x0F15)) == 0x0F15)
      assert(bytes.from_table({ 65, 66 }) == "AB" and #bytes.to_table("AB") == 2)
      print("loaded")
      log.info("still loaded")
      return {
        describe = function() return { name = "sandboxed" } end,
        decode = function(bytes) return {}, "" end,
        encode = function() return "" end,
      }
    "#;
    let codec = LuaCodec::from_source("sandbox.lua", plugin, limits()).expect("the asserts hold");
    assert_eq!(codec.info().name, "sandboxed");
}

#[test]
fn a_plugin_that_cannot_load_says_why() {
    let cases = [
        ("return {", "syntax"),
        ("return 5", "must return a table"),
        (
            "return { describe = function() end, decode = 1, encode = print }",
            "`decode`",
        ),
        (
            "return { describe = function() return { name = 'x', kinds = { { kind = 'k', fields = { { name = 'f', type = 'u8' } } } } } end, decode = print, encode = print }",
            "u8",
        ),
        (
            "return { describe = function() return {} end, decode = print, encode = print }",
            "name is missing",
        ),
        ("while true do end", "budget"),
    ];
    for (code, hint) in cases {
        match LuaCodec::from_source("bad.lua", code, limits()) {
            Err(CodecError::Internal(message)) => {
                assert!(message.contains(hint), "{code}: {message}");
                assert!(message.starts_with("bad.lua"), "{message}");
            }
            other => panic!("{code}: {other:?}"),
        }
    }
    let missing = LuaCodec::load("/nonexistent/plugin.lua").unwrap_err();
    assert!(missing.to_string().contains("/nonexistent/plugin.lua"));
}

#[test]
fn reload_picks_up_edits_and_keeps_the_old_plugin_on_failure() {
    let dir = TempDir::new("reload");
    let path = dir.write("p/plugin.lua", &LINES.replace("0.1.0", "1"));
    let mut codec = LuaCodec::load_with(&path, limits()).unwrap();
    assert_eq!(codec.info().version, "1");
    assert_eq!(codec.path(), Some(path.as_path()));
    let at = Instant::now();
    let mut out = Vec::new();
    codec.decode(b"par", at, 0, &mut out);
    assert_eq!(codec.held_back(), 3);

    dir.write("p/plugin.lua", &LINES.replace("0.1.0", "2"));
    codec.reload().unwrap();
    assert_eq!(codec.info().version, "2");
    assert_eq!(codec.held_back(), 0, "a reload starts decoding over");

    dir.write("p/plugin.lua", "return {");
    assert!(codec.reload().is_err());
    assert_eq!(codec.info().version, "2");
    codec.decode(b"still\n", at, 3, &mut out);
    assert_eq!(texts(&out), [("line", 3..9)]);
}

#[test]
fn reset_forgets_held_bytes_and_state() {
    let mut codec = lines();
    let at = Instant::now();
    let mut out = Vec::new();
    codec.decode(b"a\nb", at, 0, &mut out);
    codec.reset();
    codec.decode(b"c\n", at, 3, &mut out);
    assert_eq!(texts(&out), [("line", 0..2), ("line", 3..5)]);
    assert_eq!(out[1].field("calls"), Some(&Value::Int(1)));
    assert_eq!(out[1].summary, "c");
}

#[test]
fn plugins_are_found_by_folder_and_webassembly_is_recognised() {
    let dir = TempDir::new("plugins");
    dir.write("airoha-race/plugin.lua", serialist_plugins::AIROHA_RACE_LUA);
    dir.write("broken/plugin.lua", "return {");
    dir.write("compiled/plugin.wasm", "\0asm");
    dir.write("empty/readme.txt", "nothing here");
    let found = find_plugins(dir.path());
    let kinds: Vec<_> = found.iter().map(|p| p.kind).collect();
    assert_eq!(kinds, [PluginKind::Lua, PluginKind::Lua, PluginKind::Wasm]);

    let mut registry = serialist_plugins::builtin_registry();
    let warnings = load_plugins(dir.path(), &mut registry, LuaLimits::default());
    let warned: Vec<_> = warnings
        .iter()
        .map(|w| w.path.parent().unwrap().file_name().unwrap().to_owned())
        .collect();
    assert_eq!(warned, ["broken", "compiled"]);
    assert!(warnings[1].message.contains("WebAssembly"));
    // The Lua plugin replaced the built-in of the same name and decodes the same.
    let mut codec = registry.create("airoha-race").unwrap();
    let mut out = Vec::new();
    codec.decode(
        &[0x05, 0x5B, 0x02, 0x00, 0x15, 0x0F],
        Instant::now(),
        0,
        &mut out,
    );
    assert_eq!(out[0].kind, "response");
    assert!(find_plugins(&dir.path().join("missing")).is_empty());
}

#[test]
fn a_lua_codec_moves_to_the_ingest_thread() {
    let mut registry = CodecRegistry::new();
    registry.register(Arc::new(bundled_race_lua(LuaLimits::default()).unwrap()));
    let mut codec = registry.create("airoha-race").unwrap();
    let frames = std::thread::spawn(move || {
        let mut out = Vec::new();
        codec.decode(b"hello\n", Instant::now(), 0, &mut out);
        out
    })
    .join()
    .unwrap();
    assert_eq!(frames[0].field("text"), Some(&Value::Str("hello".into())));
}
