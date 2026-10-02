//! The Lua adapter: the plugin contract, the sandbox, the limits, reload and discovery.

mod common;

use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use serialist_core::settings::ConfigPaths;
use serialist_core::{Codec, CodecError, CodecRegistry, EncodeRequest, Frame, Severity, Value};
use serialist_plugins::race::race_info;
use serialist_plugins::{
    EXAMPLE_PLUGINS, LuaCodec, LuaLimits, PLUGIN_ERROR_KIND, PluginKind, bundled_race_lua,
    example_plugin, find_plugins, load_plugins,
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

/// A plugin whose `decode` runs `body` (Lua source, with `bytes` and `state` in scope)
/// and returns what it does. It describes the kind `k` with no fields and the kind `u`
/// with one required `uint`, `n`.
fn returning(body: &str) -> LuaCodec {
    returning_within(body, limits())
}

fn returning_within(body: &str, limits: LuaLimits) -> LuaCodec {
    let code = format!(
        r#"
        local M = {{}}
        function M.describe()
          return {{
            name = "returning",
            kinds = {{
              {{ kind = "k" }},
              {{ kind = "u", fields = {{ {{ name = "n", type = "uint" }} }} }},
            }},
          }}
        end
        function M.decode(bytes, state) {body} end
        function M.encode() return "" end
        return M
        "#
    );
    LuaCodec::from_source("returning.lua", &code, limits).expect("the test plugin loads")
}

/// The one `plugin_error` frame there must be, and what it says.
fn only_error(frames: &[Frame]) -> (std::ops::Range<u64>, String) {
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].kind, PLUGIN_ERROR_KIND, "{frames:?}");
    let message = frames[0].field("error").and_then(Value::as_str);
    (frames[0].raw.clone(), message.unwrap().to_owned())
}

#[test]
fn a_frame_position_at_the_edge_of_an_integer_is_an_error_not_an_overflow() {
    let cases = [
        ("math.maxinteger", "1"),
        ("math.maxinteger", "2"),
        ("math.maxinteger - 1", "3"),
        ("math.mininteger", "1"),
        ("math.mininteger + 1", "1"),
        ("2", "math.maxinteger"),
        ("1", "math.maxinteger"),
        ("1", "math.mininteger"),
        ("math.maxinteger", "math.maxinteger"),
    ];
    for (pos, len) in cases {
        let mut codec = returning(&format!(
            "return {{ {{ kind = 'k', pos = {pos}, len = {len} }} }}, ''"
        ));
        let frames = decode_chunks(&mut codec, &[b"abc"], Instant::now());
        let (raw, message) = only_error(&frames);
        assert_eq!(raw, 0..3, "pos {pos}, len {len}");
        assert!(message.contains("fall outside"), "{message}");
    }
    // The last byte, and the empty frame after it, still fit.
    let mut codec = returning(
        "return { { kind = 'k', pos = 3, len = 1 }, { kind = 'k', pos = 4, len = 0 } }, ''",
    );
    let frames = decode_chunks(&mut codec, &[b"abc"], Instant::now());
    assert_eq!(texts(&frames), [("k", 2..3), ("k", 3..3)]);
}

#[test]
fn a_frame_of_a_kind_describe_does_not_list_is_an_error() {
    let mut codec = returning(
        "return { { kind = 'k', pos = 1, len = 1 }, { kind = 'nope', pos = 2, len = 2 } }, ''",
    );
    let frames = decode_chunks(&mut codec, &[b"abcd"], Instant::now());
    assert_eq!(texts(&frames), [("k", 0..1), (PLUGIN_ERROR_KIND, 1..3)]);
    let message = frames[1].field("error").and_then(Value::as_str).unwrap();
    assert!(
        message.contains("`nope`") || message.contains("\"nope\""),
        "{message}"
    );
    assert!(message.contains("describe()"), "{message}");
}

/// A plugin can share one table between many places, so a few lines of Lua describe a
/// list of lists of lists that holds hundreds of millions of values. What the host
/// copies out of it is capped per call, however it is shaped.
#[test]
fn a_returned_graph_cannot_be_bigger_than_the_budget() {
    let mut codec = returning(
        r#"
        if bytes:sub(1, 1) == "b" then
          local t = 1
          for _ = 1, 12 do
            local n = {}
            for i = 1, 13 do n[i] = t end
            t = n
          end
          return {
            { kind = "k", pos = 1, len = 1, fields = { x = t } },
            { kind = "k", pos = 2, len = 1 },
          }, ""
        end
        return { { kind = "k", pos = 1, len = 1, fields = { x = { 1, 2, 3 } } } }, ""
        "#,
    );
    let started = Instant::now();
    let frames = decode_chunks(&mut codec, &[b"bb", b"ok"], started);
    assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
    // The graph spent the call's budget: its frame, and every frame after it in that
    // call, are plugin errors. The next call starts with a full budget.
    assert_eq!(
        texts(&frames),
        [
            (PLUGIN_ERROR_KIND, 0..1),
            (PLUGIN_ERROR_KIND, 1..2),
            ("k", 2..3)
        ]
    );
    let message = frames[0].field("error").and_then(Value::as_str).unwrap();
    assert!(message.contains("hold more than"), "{message}");
    assert_eq!(
        frames[2].field("x"),
        Some(&Value::List(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3)
        ]))
    );
}

#[test]
fn text_shared_by_many_frames_is_charged_every_time() {
    // 80 frames sharing one 1 MiB summary: 80 MiB once copied out, against a budget of
    // 64 MiB, so the first frames come out and the rest are errors.
    let mut codec = returning(
        r#"
        local text = string.rep("x", 1 << 20)
        local frames = {}
        for i = 1, 80 do
          frames[i] = { kind = "k", pos = 1, len = 1, summary = text }
        end
        return frames, ""
        "#,
    );
    let frames = decode_chunks(&mut codec, &[b"a"], Instant::now());
    assert_eq!(frames.len(), 80);
    let ok = frames.iter().take_while(|f| f.kind == "k").count();
    assert!((60..=64).contains(&ok), "{ok} frames came out whole");
    assert!(frames[ok..].iter().all(|f| f.kind == PLUGIN_ERROR_KIND));
    assert_eq!(frames[0].summary.len(), 1 << 20);
}

#[test]
fn the_frame_budget_is_a_limit_like_the_others() {
    assert_eq!(LuaLimits::default().max_frame_bytes, 64 * 1024 * 1024);
    let limits = LuaLimits {
        max_frame_bytes: 1000,
        ..limits()
    };
    // A frame costs 32 bytes and the length of its kind and summary: the first 800 fits,
    // the second does not, and the next call starts with a full budget again.
    let body = r#"
        local s = string.rep("x", 800)
        return {
          { kind = "k", pos = 1, len = 1, summary = s },
          { kind = "k", pos = 2, len = 1, summary = s },
        }, "" "#;
    let mut codec = returning_within(body, limits);
    let frames = decode_chunks(&mut codec, &[b"ab", b"cd"], Instant::now());
    assert_eq!(
        texts(&frames),
        [
            ("k", 0..1),
            (PLUGIN_ERROR_KIND, 1..2),
            ("k", 2..3),
            (PLUGIN_ERROR_KIND, 3..4)
        ]
    );
    let message = frames[1].field("error").and_then(Value::as_str).unwrap();
    assert!(message.contains("more than 1000 bytes"), "{message}");
}

#[test]
fn field_names_that_differ_only_in_invalid_utf8_come_out_in_byte_order() {
    // Both names read as U+FFFD once converted: the bytes decide the order, not the
    // order Lua keeps its table in.
    let mut codec = returning(
        r#"return { { kind = "k", pos = 1, len = 1, fields = {
          ["\xff"] = 1, ["\xfe"] = 2, ["\xfd"] = 3, a = 0,
        } } }, """#,
    );
    let frames = decode_chunks(&mut codec, &[b"a"], Instant::now());
    let values: Vec<_> = frames[0].fields.iter().map(|(_, v)| v.clone()).collect();
    assert_eq!(
        values,
        [Value::Int(0), Value::Int(3), Value::Int(2), Value::Int(1)]
    );
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

    let mut registry = CodecRegistry::new();
    let warnings = load_plugins(dir.path(), &mut registry, LuaLimits::default());
    let warned: Vec<_> = warnings
        .iter()
        .map(|w| w.path.parent().unwrap().file_name().unwrap().to_owned())
        .collect();
    assert_eq!(warned, ["broken", "compiled"]);
    assert!(warnings[1].message.contains("WebAssembly"));
    // The Lua plugin is the only codec, registered under the name it describes.
    assert_eq!(registry.names().collect::<Vec<_>>(), ["airoha-race"]);
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

/// The bundled examples decode nothing until installed; installed, they load from
/// `plugins/` like any plugin, and the RACE one describes itself as the reference does.
#[test]
fn the_example_plugins_load_once_installed() {
    let dir = TempDir::new("examples");
    let paths = ConfigPaths::new(dir.path());
    let plugins = paths.plugins_dir();
    let loaded = |registry: &mut CodecRegistry| {
        let warnings = load_plugins(&plugins, registry, LuaLimits::default());
        assert!(warnings.is_empty(), "{warnings:?}");
    };
    let mut registry = CodecRegistry::new();
    paths.ensure_example_plugins(EXAMPLE_PLUGINS).unwrap();
    loaded(&mut registry);
    assert_eq!(
        registry.names().count(),
        0,
        "shipped, not installed: plugins/examples/ is not a plugin"
    );

    for example in EXAMPLE_PLUGINS {
        paths.install_example_plugin(example).unwrap();
    }
    let folders: Vec<String> = find_plugins(&plugins)
        .iter()
        .map(|plugin| {
            plugin
                .dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let names: Vec<&str> = EXAMPLE_PLUGINS.iter().map(|example| example.name).collect();
    assert_eq!(folders, names);
    assert_eq!(names[0], "airoha-race");
    assert_eq!(
        example_plugin("airoha-race").map(|e| e.title),
        Some("Airoha RACE")
    );
    loaded(&mut registry);
    let factory = registry.get("airoha-race").expect("the RACE example");
    assert_eq!(factory.info(), race_info());
    let mut out = Vec::new();
    factory.create().unwrap().decode(
        &[0x05, 0x5B, 0x02, 0x00, 0x15, 0x0F],
        Instant::now(),
        0,
        &mut out,
    );
    assert_eq!(out[0].kind, "response");
}

/// A Lua codec never crosses threads: its factory does, and the codec is made on the
/// thread that runs it.
#[test]
fn a_lua_codec_is_made_on_the_thread_that_runs_it() {
    let mut registry = CodecRegistry::new();
    registry.register(Arc::new(bundled_race_lua(LuaLimits::default()).unwrap()));
    let factory = registry.get("airoha-race").unwrap();
    let frames = std::thread::spawn(move || {
        let mut codec = factory.create().unwrap();
        let mut out = Vec::new();
        codec.decode(b"hello\n", Instant::now(), 0, &mut out);
        out
    })
    .join()
    .unwrap();
    assert_eq!(frames[0].field("text"), Some(&Value::Str("hello".into())));
}
