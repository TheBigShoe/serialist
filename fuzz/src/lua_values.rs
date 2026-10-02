//! `lua_values`: the boundary untrusted Lua plugins cross, in
//! `crates/serialist-plugins/src/lua/convert.rs` and the adapter around it. A fixed test
//! plugin (`lua_values.lua`) returns whatever its input bytes say, so the fuzzer chooses
//! the Lua values the adapter converts into frames and encode results, and the Rust
//! values it converts into the Lua request. A model written from the plugin contract
//! (the docs of `serialist_plugins::lua`) says what must come out; the target compares.
//!
//! The input is an [`Input`]. Config bits 0..=1 pick the mode; config 0, decode, is the
//! one a real session exercises:
//!
//! - **0, decode.** Each chunk is a program for `decode`; held-back bytes are put in
//!   front of the next chunk, as the adapter does.
//! - **1, encode, program.** The stream is one program; `encode` of the command `run`
//!   returns its two values.
//! - **2, encode, request.** The stream is a JSON object, a saved command's fields (see
//!   [`codec::encode_request`]); `encode` of the command `echo` returns the request as
//!   the plugin received it, written out as a canonical string. Anything that is not
//!   such an object is skipped, so seeds in this mode start `\x02\x00{`.
//! - **3,** decode, like 0.
//!
//! # The programs
//!
//! A program is a bytecode read by a recursive machine, one value per call. Every byte
//! sequence is a program: a byte past the end reads as 0, which is `nil`, and each
//! operand is reduced modulo its range. The opcode is the first byte modulo 20:
//!
//! | op | name | operands | value |
//! |---|---|---|---|
//! | 0 | nil | | `nil` |
//! | 1, 2 | false, true | | a boolean |
//! | 3 | int | `i8` | an integer |
//! | 4 | edge | `u8` | `EDGES[k % 8]`: `maxinteger`, `mininteger`, one in from each, `1<<31`, `-(1<<31)`, `1<<53`, 255 |
//! | 5 | float | 8 bytes, little endian | any float, NaNs included |
//! | 6 | special | `u8` | `SPECIALS[k % 16]`: NaN, the infinities, `-0.0`, 2^63, -2^63, 2^53, and others at the edges of what converts to an integer |
//! | 7 | str | `n`, `n` bytes | a string of any bytes, UTF-8 or not |
//! | 8 | name | `u8` | `NAMES[k % 32]`: the frame kinds `plain`, `typed`, `loose` and `other`, severities, field names, the keys of a frame and of an error table |
//! | 9 | array | `n % 17`, `n` values | a table with those values at 1..n (a `nil` leaves a hole) |
//! | 10 | record | `n % 9`, `n` key and value pairs | a table; a `nil` or NaN key raises |
//! | 11 | share | `n % 17`, one value | a table holding that one value `n` times: a graph, not a tree |
//! | 12 | def | as record | a record that later `ref`s can name, its own children included: cycles |
//! | 13 | ref | `u8` | the `k % count`th table `def` made, or `nil` when there is none |
//! | 14, 15 | func, thread | | a function, a coroutine |
//! | 16 | size | | the length of the bytes `decode` was given |
//! | 17 | frame | six values | `{kind, pos, len, severity, summary, fields}`, a `nil` leaving its key out |
//! | 18 | tail | `u8` | the last `k` bytes of the input, or all of it |
//! | 19 | raise | `u8` | raises an error: a string, a table or `nil` |
//!
//! Past depth 48 a value is `nil` without reading anything. `decode` builds two values
//! from the program, the frames and the bytes to hold back; `encode` builds its two
//! results. A valid frame in a list, `array 1 (frame (name 0) (int 1) size nil nil
//! nil)`, then no bytes to hold back, is `\x09\x01\x11\x08\x00\x03\x01\x10\x00\x00\x00`
//! and every seed is some variation on it.
//!
//! # The model
//!
//! [`Machine`] runs the same program in Rust, on a table model that follows Lua 5.4's
//! rules for keys (a float with a whole value is an integer key, `nil` and NaN keys
//! raise, a `nil` value removes the key). The conversion model is written from the
//! contract in the docs of `serialist_plugins::lua` (and of `Budget` in `convert.rs`),
//! not from the code that converts:
//!
//! - `decode` raising, or its two values being malformed, gives one `plugin_error` frame
//!   covering all the bytes it was given.
//! - A frame is a table with a string `kind` that `describe` lists, integer `pos` and
//!   `len` that fit the bytes, an optional `severity` (`info`, `warning`, `error`) and
//!   `summary`, and optional `fields`. A frame whose conversion fails becomes a
//!   `plugin_error` frame over its own bytes and the others still come out; but a list
//!   that is not a list of frames with positions that fit, fails the whole call.
//! - Declared fields come first, in declared order and in their declared types (a
//!   string is `bytes` or `str`; whole floats and integers are `int` or `uint`; a
//!   `uint` is not negative); a declared field that is not optional must be there.
//!   The rest follow sorted by name (and by the bytes of the name where invalid UTF-8
//!   makes two the same text), their types inferred. Field names are strings. Lists
//!   nest at most 32 deep and hold the table's sequence, up to its first `nil`.
//! - The frames of one call may take [`FRAME_BYTES`] between them, as the `Budget` docs in
//!   `convert.rs` count it (the model repeats the charges, in the same order, because
//!   what a failed frame has spent decides what the next one has left): a frame costs
//!   32 and the length of its `kind`, `severity` and `summary` text; each pair of its
//!   `fields` 32 and the length of the key; each value 32, and the length of a string
//!   or bytes value, a list's items being values. A charge that does not fit empties
//!   the budget and fails the frame it was for, so every later frame of the call fails
//!   too. Sharing one table many times (opcode 11) is how a 40-byte program makes
//!   hundreds of millions of values.
//! - The bytes `decode` holds back must be a suffix of its input and at most
//!   [`HELD_BACK_LIMIT`].
//! - `encode` returns a string or a byte table (integers or whole floats from 0 to
//!   255) as the bytes; `nil` and an error table `codec.unknown_command`,
//!   `codec.missing_field` or `codec.bad_field` made as the matching [`CodecError`];
//!   anything else is [`CodecError::Internal`].
//! - A request reaches the plugin as JSON turned into Lua values: integers that fit an
//!   `i64`, other numbers as floats, `null` as `codec.null`, arrays marked for
//!   `codec.is_array`, objects as tables, nested at most 32 deep.
//!
//! # Checks
//!
//! Every call goes through [`codec::decode_chunks`], which holds the adapter to the
//! `Codec` contract, whatever the plugin returns. On top of that the frames equal the
//! model's (every field, `at` included, floats by their bits and any NaN equal to any
//! other), the bytes held back are the model's, and an encode gives the model's bytes
//! or the model's kind of error. Each run makes its own codec: a VM reused across
//! inputs would make a crash impossible to reproduce from its input. A run stops feeding
//! chunks once its calls have spent half a call's budget between them ([`WORK_LIMIT`]),
//! so the many chunks of one input cannot add up to more work or memory than a hostile
//! plugin could already ask for in one call.
//!
//! # Known deviation
//!
//! The `Codec` contract says frames come out in order of `raw.start`. The Lua adapter
//! does not keep it in two cases. When a plugin's answer turns out bad after some of its
//! frames were converted (an item that is not a frame, a position that does not fit, a
//! suffix that is not one or is too long), the `plugin_error` frame for the call comes
//! after those frames, and starts at or before them. And frames a plugin returns out of
//! order are passed on as they are. The adapter's test
//! `frames_that_break_the_contract_are_reported_not_trusted` asserts the first, so it is
//! not fixed here (a fix is to convert the frames into a scratch list, and to put into
//! `out` either all of them or only the failure frame). When the model predicts frames
//! out of order, the codec runs without [`codec::decode_chunks`]'s checks and is still
//! held to the model, so any other change in what it does is a failure.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use serialist_core::codec::{
    Codec, CodecError, EncodeRequest, FieldType, Frame, Severity, SmolStr, Value, encode_hex,
};
use serialist_plugins::{LuaCodec, LuaCodecFactory, LuaLimits, PLUGIN_ERROR_KIND};

use crate::{Input, codec};

/// The test plugin: see its header for what each command does.
const PLUGIN: &str = include_str!("lua_values.lua");

/// The most bytes the plugin may hold back (`LuaLimits::max_held_back`). Small, so a
/// 4 KiB input can go past it.
pub const HELD_BACK_LIMIT: usize = 16;

/// Deepest nesting of lists in a frame and of arrays and objects in a request that the
/// adapter converts (`MAX_DEPTH` in `convert.rs`: the contract says "nested at most 32
/// deep" for both).
const MAX_NESTING: usize = 32;

/// What the frames of one decode call may cost together: `LuaLimits::max_frame_bytes`,
/// which the target sets far below its default of 64 MiB. A call that spends it all
/// (opcode 11 does that in a few bytes) then takes milliseconds and a few megabytes, as
/// libFuzzer's time and memory limits need, and still ends in the same plugin error.
const FRAME_BYTES: usize = 4 * 1024 * 1024;

/// What a frame, a field name or a value costs besides its text (`UNIT` in
/// `convert.rs`; the charges are listed in the module docs).
const UNIT: usize = 32;

/// How much the calls of one run may spend of their budgets, taken together, before the
/// run stops feeding chunks. The app's frame store keeps frames by count, not by size,
/// so a run of many calls that each keep a full budget of frames would only report
/// that, and a few hundred of them take longer than libFuzzer's time limit. Both the
/// codec and the model get the chunks fed before the limit.
const WORK_LIMIT: usize = FRAME_BYTES / 2;

/// What is left of the budget. `Err` when a charge does not fit, which empties it.
struct Budget(usize);

impl Budget {
    fn spend(&mut self, bytes: usize) -> Result<(), ()> {
        match self.0.checked_sub(bytes) {
            Some(left) => {
                self.0 = left;
                Ok(())
            }
            None => {
                self.0 = 0;
                Err(())
            }
        }
    }
}

// The machine's constants: the same as in lua_values.lua.
const MAX_DEPTH: usize = 48;
const NAMES: [&str; 32] = [
    "plain",
    "typed",
    "loose",
    "other",
    "info",
    "warning",
    "error",
    "fatal",
    "b",
    "i",
    "u",
    "f",
    "s",
    "y",
    "l",
    "o",
    "a",
    "x",
    "kind",
    "pos",
    "len",
    "severity",
    "summary",
    "fields",
    "code",
    "unknown_command",
    "missing_field",
    "bad_field",
    "command",
    "field",
    "reason",
    "",
];
const EDGES: [i64; 8] = [
    i64::MAX,
    i64::MIN,
    i64::MAX - 1,
    i64::MIN + 1,
    1 << 31,
    -(1 << 31),
    1 << 53,
    255,
];
const SPECIALS: [f64; 16] = [
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    -0.0,
    9_223_372_036_854_775_808.0,
    -9_223_372_036_854_775_808.0,
    9_007_199_254_740_992.0,
    0.5,
    -1.0,
    3.0,
    1e300,
    255.0,
    256.0,
    9_223_372_036_854_774_784.0,
    4_294_967_296.0,
    1.5,
];

/// The fields the plugin's `describe` declares for each kind: name, type, optional.
/// `the_model_schema_is_what_describe_says` holds this to the plugin.
type Declared = [(&'static str, FieldType, bool)];
const SCHEMA: [(&str, &Declared); 3] = [
    ("plain", &[]),
    (
        "typed",
        &[
            ("b", FieldType::Bool, false),
            ("i", FieldType::Int, false),
            ("u", FieldType::UInt, false),
            ("f", FieldType::Float, false),
            ("s", FieldType::Str, false),
            ("y", FieldType::Bytes, false),
            ("l", FieldType::List, false),
            ("o", FieldType::UInt, true),
        ],
    ),
    (
        "loose",
        &[("a", FieldType::Int, true), ("s", FieldType::Str, true)],
    ),
];

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    match input.pick(0, &[0u8, 1, 2, 0]) {
        1 => encode_program(input.stream),
        2 => encode_request(input.stream),
        _ => decode(&input),
    }
}

/// A fresh codec for the test plugin. The factory is built once, which loads the plugin
/// to describe it; every codec is a new VM.
fn new_codec() -> LuaCodec {
    static FACTORY: OnceLock<LuaCodecFactory> = OnceLock::new();
    FACTORY
        .get_or_init(|| {
            let limits = LuaLimits {
                memory_bytes: 16 << 20,
                max_held_back: HELD_BACK_LIMIT,
                max_frame_bytes: FRAME_BYTES,
                ..LuaLimits::default()
            };
            LuaCodecFactory::from_source("lua_values.lua", PLUGIN, limits)
                .expect("the test plugin loads")
        })
        .create_lua()
        .expect("the test plugin loads")
}

// ---------------------------------------------------------------------------------
// Decode

fn decode(input: &Input) {
    let t0 = Instant::now();
    let mut model = Model::default();
    let mut expected = Vec::new();
    let mut chunks = Vec::new();
    let mut spent = 0;
    for (i, chunk) in input.chunks().enumerate() {
        if spent >= WORK_LIMIT {
            break;
        }
        spent += model.decode(chunk, t0 + Duration::from_millis(i as u64), &mut expected);
        chunks.push(chunk);
    }

    let mut codec = new_codec();
    // The one place the adapter is known to break the `Codec` contract (see the module
    // docs): when the model says frames come out of order, run the codec without the
    // contract checks, and still hold it to the model.
    let in_order = expected
        .windows(2)
        .all(|pair| pair[0].start() <= pair[1].start());
    let frames = if in_order {
        codec::decode_chunks(&mut codec, chunks.iter().copied(), t0)
    } else {
        decode_unchecked(&mut codec, &chunks, t0)
    };
    assert_eq!(
        frames.len(),
        expected.len(),
        "the codec made {} frames, the model {}:\n{frames:#?}\n{expected:#?}",
        frames.len(),
        expected.len()
    );
    for (i, (frame, want)) in frames.iter().zip(&expected).enumerate() {
        if let Err(why) = want.matches(frame) {
            panic!("frame {i}: {why}\nthe codec made {frame:#?}\nthe model wanted {want:#?}");
        }
    }
    assert_eq!(
        codec.held_back(),
        model.held.len(),
        "bytes held back: the codec {}, the model {}",
        codec.held_back(),
        model.held.len()
    );
}

/// [`codec::decode_chunks`] without its checks, for the inputs that break them.
fn decode_unchecked(codec: &mut LuaCodec, chunks: &[&[u8]], t0: Instant) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    for (i, chunk) in chunks.iter().enumerate() {
        codec.decode(
            chunk,
            t0 + Duration::from_millis(i as u64),
            offset,
            &mut out,
        );
        offset += chunk.len() as u64;
    }
    out
}

/// What the model expects of one frame.
#[derive(Debug)]
enum Expect {
    /// A frame the plugin made, as converted.
    Frame(Frame),
    /// A `plugin_error` frame over these bytes, at this time.
    Failure(Range<u64>, Instant),
}

impl Expect {
    fn start(&self) -> u64 {
        match self {
            Expect::Failure(raw, _) => raw.start,
            Expect::Frame(frame) => frame.raw.start,
        }
    }

    fn matches(&self, got: &Frame) -> Result<(), String> {
        match self {
            Expect::Failure(raw, at) => {
                if got.kind != PLUGIN_ERROR_KIND {
                    return Err(format!("not a {PLUGIN_ERROR_KIND} frame"));
                }
                let error = got.field("error");
                if got.raw != *raw
                    || got.at != *at
                    || got.severity != Severity::Error
                    || !got.summary.starts_with("plugin error: ")
                    || !matches!(error, Some(Value::Str(_)))
                    || got.fields.len() != 1
                {
                    return Err("a plugin error frame, but not the one wanted".to_owned());
                }
                Ok(())
            }
            Expect::Frame(want) => {
                if got.kind != want.kind
                    || got.raw != want.raw
                    || got.at != want.at
                    || got.direction != want.direction
                    || got.severity != want.severity
                    || got.summary != want.summary
                {
                    return Err("kind, raw, time, severity or summary differ".to_owned());
                }
                same_fields(&got.fields, &want.fields)
            }
        }
    }
}

/// Field lists equal: the same names and values in the same order.
fn same_fields(got: &[(SmolStr, Value)], want: &[(SmolStr, Value)]) -> Result<(), String> {
    if got.len() != want.len() {
        return Err(format!(
            "{} fields, wanted {}: {got:?}",
            got.len(),
            want.len()
        ));
    }
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        if g.0 != w.0 || !same_value(&g.1, &w.1) {
            return Err(format!("field {i} is {g:?}, wanted {w:?}"));
        }
    }
    Ok(())
}

/// Values equal, floats by their bits except that every NaN is the same.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => {
            (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()
        }
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_value(x, y))
        }
        _ => a == b,
    }
}

/// What the adapter must do with the chunks, from the contract.
#[derive(Default)]
struct Model {
    held: Vec<u8>,
    /// Stream offset of the next chunk.
    offset: u64,
}

impl Model {
    /// What the adapter must do with `chunk`, pushing the frames; returns how much of the
    /// call's budget the frames spent.
    fn decode(&mut self, chunk: &[u8], at: Instant, out: &mut Vec<Expect>) -> usize {
        if chunk.is_empty() {
            return 0;
        }
        let base = self.offset.saturating_sub(self.held.len() as u64);
        let mut input = std::mem::take(&mut self.held);
        input.extend_from_slice(chunk);
        self.offset += chunk.len() as u64;
        let end = base + input.len() as u64;
        let failure = |raw| Expect::Failure(raw, at);

        let mut machine = Machine::new(&input, &input);
        let values = machine
            .value(0)
            .and_then(|frames| machine.value(0).map(|rest| (frames, rest)));
        let Ok((frames, rest)) = values else {
            out.push(failure(base..end));
            return 0;
        };
        let mut budget = Budget(FRAME_BYTES);
        let collected = machine.collect_frames(&frames, base, at, input.len(), &mut budget, out);
        if collected.is_err() {
            out.push(failure(base..end));
        }
        match rest {
            M::Nil => {}
            M::Str(rest) => {
                if !input.ends_with(&rest) {
                    out.push(failure(base..end));
                } else if rest.len() > HELD_BACK_LIMIT {
                    out.push(failure(end - rest.len() as u64..end));
                } else {
                    self.held.extend_from_slice(&rest);
                }
            }
            _ => out.push(failure(base..end)),
        }
        FRAME_BYTES - budget.0
    }
}

// ---------------------------------------------------------------------------------
// The machine and the conversion model

/// A Lua value, tables by index into the machine's arena (so a table can hold itself).
#[derive(Clone, Debug)]
enum M {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Table(usize),
    Func,
    Thread,
}

/// A table key as Lua stores it: whole floats are integers, and every other kind of key
/// only matters as "not an integer and not a string".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Int(i64),
    Str(Vec<u8>),
    Other(u8, u64),
}

/// The program raised an error.
struct Raise;

struct Machine<'a> {
    program: &'a [u8],
    input: &'a [u8],
    pos: usize,
    tables: Vec<BTreeMap<Key, M>>,
    /// The tables `def` made, in order.
    env: Vec<usize>,
    /// A counter that makes every function and coroutine a different key.
    fresh: u64,
}

/// A whole float that fits an `i64`, as Lua converts it to an integer key; the same
/// rule as `convert.rs` uses for integers in fields.
fn whole(x: f64) -> Option<i64> {
    (x.fract() == 0.0 && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&x))
        .then_some(x as i64)
}

fn integer(value: &M) -> Option<i64> {
    match *value {
        M::Int(n) => Some(n),
        M::Float(x) => whole(x),
        _ => None,
    }
}

impl<'a> Machine<'a> {
    fn new(program: &'a [u8], input: &'a [u8]) -> Self {
        Self {
            program,
            input,
            pos: 0,
            tables: Vec::new(),
            env: Vec::new(),
            fresh: 0,
        }
    }

    fn u8(&mut self) -> u8 {
        let byte = self.program.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        byte
    }

    fn table(&mut self) -> usize {
        self.tables.push(BTreeMap::new());
        self.tables.len() - 1
    }

    fn key(&mut self, key: &M) -> Result<Key, Raise> {
        Ok(match key {
            M::Nil => return Err(Raise),
            M::Bool(b) => Key::Other(0, u64::from(*b)),
            M::Int(n) => Key::Int(*n),
            M::Float(x) if x.is_nan() => return Err(Raise),
            M::Float(x) => whole(*x).map_or(Key::Other(1, x.to_bits()), Key::Int),
            M::Str(s) => Key::Str(s.clone()),
            M::Table(t) => Key::Other(2, *t as u64),
            M::Func | M::Thread => {
                self.fresh += 1;
                Key::Other(3, self.fresh)
            }
        })
    }

    /// `t[key] = value`.
    fn set(&mut self, t: usize, key: &M, value: M) -> Result<(), Raise> {
        let key = self.key(key)?;
        if matches!(value, M::Nil) {
            self.tables[t].remove(&key);
        } else {
            self.tables[t].insert(key, value);
        }
        Ok(())
    }

    fn entries(&mut self, t: usize, n: usize, depth: usize) -> Result<(), Raise> {
        for _ in 0..n {
            let key = self.value(depth + 1)?;
            let value = self.value(depth + 1)?;
            self.set(t, &key, value)?;
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<M, Raise> {
        if depth >= MAX_DEPTH {
            return Ok(M::Nil);
        }
        Ok(match self.u8() % 20 {
            0 => M::Nil,
            1 => M::Bool(false),
            2 => M::Bool(true),
            3 => M::Int(i64::from(self.u8() as i8)),
            4 => M::Int(EDGES[usize::from(self.u8()) % EDGES.len()]),
            5 => {
                let bytes: [u8; 8] = std::array::from_fn(|_| self.u8());
                M::Float(f64::from_le_bytes(bytes))
            }
            6 => M::Float(SPECIALS[usize::from(self.u8()) % SPECIALS.len()]),
            7 => {
                let n = usize::from(self.u8());
                let start = self.pos.min(self.program.len());
                let end = (self.pos + n).min(self.program.len());
                self.pos += n;
                M::Str(self.program[start..end].to_vec())
            }
            8 => M::Str(NAMES[usize::from(self.u8()) % NAMES.len()].into()),
            9 => {
                let t = self.table();
                for i in 1..=usize::from(self.u8()) % 17 {
                    let value = self.value(depth + 1)?;
                    self.set(t, &M::Int(i as i64), value)?;
                }
                M::Table(t)
            }
            10 => {
                let t = self.table();
                let n = usize::from(self.u8()) % 9;
                self.entries(t, n, depth)?;
                M::Table(t)
            }
            11 => {
                let n = usize::from(self.u8()) % 17;
                let value = self.value(depth + 1)?;
                let t = self.table();
                for i in 1..=n {
                    self.set(t, &M::Int(i as i64), value.clone())?;
                }
                M::Table(t)
            }
            12 => {
                let t = self.table();
                self.env.push(t);
                let n = usize::from(self.u8()) % 9;
                self.entries(t, n, depth)?;
                M::Table(t)
            }
            13 => {
                let k = usize::from(self.u8());
                if self.env.is_empty() {
                    M::Nil
                } else {
                    M::Table(self.env[k % self.env.len()])
                }
            }
            14 => M::Func,
            15 => M::Thread,
            16 => M::Int(self.input.len() as i64),
            17 => {
                let t = self.table();
                for name in ["kind", "pos", "len", "severity", "summary", "fields"] {
                    let value = self.value(depth + 1)?;
                    self.set(t, &M::Str(name.into()), value)?;
                }
                M::Table(t)
            }
            18 => {
                let k = usize::from(self.u8());
                M::Str(self.input[self.input.len().saturating_sub(k)..].to_vec())
            }
            _ => return Err(Raise),
        })
    }

    /// `table[name]` for a string key, `Nil` when absent.
    fn field(&self, t: usize, name: &str) -> M {
        self.tables[t]
            .get(&Key::Str(name.as_bytes().to_vec()))
            .cloned()
            .unwrap_or(M::Nil)
    }

    /// The values at 1, 2, 3, ... up to the first `nil`: what a plugin means by a list.
    fn sequence(&self, t: usize) -> impl Iterator<Item = &M> {
        (1i64..).map_while(move |i| self.tables[t].get(&Key::Int(i)))
    }

    /// Convert the frames `decode` returned, pushing what comes out. `Err` when the list
    /// itself is malformed, which fails the whole call.
    fn collect_frames(
        &self,
        frames: &M,
        base: u64,
        at: Instant,
        input_len: usize,
        budget: &mut Budget,
        out: &mut Vec<Expect>,
    ) -> Result<(), ()> {
        let t = match frames {
            M::Nil => return Ok(()),
            M::Table(t) => *t,
            _ => return Err(()),
        };
        for item in self.sequence(t) {
            let M::Table(item) = item else {
                return Err(());
            };
            let (start, len) = self.span(*item, input_len)?;
            let raw = base + start as u64..base + (start + len) as u64;
            out.push(match self.frame(*item, raw.clone(), at, budget) {
                Ok(frame) => Expect::Frame(frame),
                Err(()) => Expect::Failure(raw, at),
            });
        }
        Ok(())
    }

    /// Where a frame sits in the input: `pos` from 1 and `len`, whole numbers that fit.
    fn span(&self, t: usize, input_len: usize) -> Result<(usize, usize), ()> {
        let pos = integer(&self.field(t, "pos")).ok_or(())?;
        let len = integer(&self.field(t, "len")).ok_or(())?;
        let start = i128::from(pos) - 1;
        let len = i128::from(len);
        if start < 0 || len < 0 || start + len > input_len as i128 {
            return Err(());
        }
        Ok((start as usize, len as usize))
    }

    fn frame(
        &self,
        t: usize,
        raw: Range<u64>,
        at: Instant,
        budget: &mut Budget,
    ) -> Result<Frame, ()> {
        budget.spend(UNIT)?;
        let M::Str(kind) = self.field(t, "kind") else {
            return Err(());
        };
        budget.spend(kind.len())?;
        let kind = String::from_utf8_lossy(&kind).into_owned();
        let severity = match self.field(t, "severity") {
            M::Nil => Severity::Info,
            M::Str(name) => {
                budget.spend(name.len())?;
                Severity::from_name(&String::from_utf8_lossy(&name)).ok_or(())?
            }
            _ => return Err(()),
        };
        let summary = match self.field(t, "summary") {
            M::Nil => String::new(),
            M::Str(text) => {
                budget.spend(text.len())?;
                String::from_utf8_lossy(&text).into_owned()
            }
            _ => return Err(()),
        };
        let (_, declared) = SCHEMA.iter().find(|(name, _)| *name == kind).ok_or(())?;
        let fields = match self.field(t, "fields") {
            M::Nil => {
                if declared.iter().any(|f| !f.2) {
                    return Err(());
                }
                Vec::new()
            }
            M::Table(fields) => self.fields(fields, declared, budget)?,
            _ => return Err(()),
        };
        let mut frame = Frame::new(kind.as_str(), raw, at)
            .with_severity(severity)
            .with_summary(summary);
        frame.fields = fields
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect();
        Ok(frame)
    }

    /// The declared fields in order, then the others by name (and by their bytes where
    /// the names are the same text once converted).
    fn fields(
        &self,
        t: usize,
        declared: &Declared,
        budget: &mut Budget,
    ) -> Result<Vec<(String, Value)>, ()> {
        let mut given = Vec::new();
        let mut bad_key = false;
        for (key, value) in &self.tables[t] {
            if let Key::Str(name) = key {
                budget.spend(UNIT + name.len())?;
                given.push((name, value));
            } else {
                budget.spend(UNIT)?;
                bad_key = true;
            }
        }
        if bad_key {
            return Err(());
        }
        let mut used = vec![false; given.len()];
        let mut out = Vec::new();
        for &(name, ty, optional) in declared {
            match given
                .iter()
                .position(|(key, _)| key.as_slice() == name.as_bytes())
            {
                Some(i) => {
                    used[i] = true;
                    out.push((
                        name.to_owned(),
                        self.value_of(given[i].1, Some(ty), 0, budget)?,
                    ));
                }
                None if optional => {}
                None => return Err(()),
            }
        }
        let mut extra: Vec<_> = given
            .iter()
            .zip(&used)
            .filter(|(_, used)| !**used)
            .map(|(&(key, value), _)| (String::from_utf8_lossy(key).into_owned(), key, value))
            .collect();
        extra.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        for (name, _, value) in extra {
            out.push((name, self.value_of(value, None, 0, budget)?));
        }
        Ok(out)
    }

    /// A value as a field value of type `ty`, or of the type it has when the kind
    /// declares none.
    fn value_of(
        &self,
        value: &M,
        ty: Option<FieldType>,
        depth: usize,
        budget: &mut Budget,
    ) -> Result<Value, ()> {
        budget.spend(UNIT)?;
        Ok(match (ty, value) {
            (Some(FieldType::Bytes), M::Str(s)) => {
                budget.spend(s.len())?;
                Value::Bytes(s.clone())
            }
            (Some(FieldType::Str) | None, M::Str(s)) => {
                budget.spend(s.len())?;
                Value::Str(String::from_utf8_lossy(s).into_owned())
            }
            (Some(FieldType::UInt), _) => {
                Value::UInt(u64::try_from(integer(value).ok_or(())?).map_err(|_| ())?)
            }
            (Some(FieldType::Int), _) => Value::Int(integer(value).ok_or(())?),
            (Some(FieldType::Float), M::Int(n)) => Value::Float(*n as f64),
            (Some(FieldType::Float) | None, M::Float(x)) => Value::Float(*x),
            (Some(FieldType::Bool) | None, M::Bool(b)) => Value::Bool(*b),
            (None, M::Int(n)) => Value::Int(*n),
            (Some(FieldType::List) | None, M::Table(t)) => {
                if depth >= MAX_NESTING {
                    return Err(());
                }
                self.sequence(*t)
                    .map(|item| self.value_of(item, None, depth + 1, budget))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::List)?
            }
            _ => return Err(()),
        })
    }
}

// ---------------------------------------------------------------------------------
// Encode

/// What `encode` must give for a request: the bytes, or an error of this kind.
#[derive(Debug, PartialEq)]
enum Want {
    Bytes(Vec<u8>),
    UnknownCommand(String),
    MissingField(String),
    BadField(String, String),
    /// Any other error; the message is the adapter's own.
    Internal,
}

fn check_encode(got: Result<Vec<u8>, CodecError>, want: &Want, what: &str) {
    let got = match got {
        Ok(bytes) => Want::Bytes(bytes),
        Err(CodecError::UnknownCommand(c)) => Want::UnknownCommand(c),
        Err(CodecError::MissingField(f)) => Want::MissingField(f),
        Err(CodecError::BadField { field, reason }) => Want::BadField(field, reason),
        Err(_) => Want::Internal,
    };
    assert_eq!(&got, want, "encode of {what}");
}

/// Mode 1: `encode` of `run` returns the two values the program builds.
fn encode_program(program: &[u8]) {
    let request = EncodeRequest::new("run").with("program", encode_hex(program, ""));
    let got = new_codec().encode(&request);
    check_encode(got, &expected_result(program), "a program");
}

fn expected_result(program: &[u8]) -> Want {
    let mut machine = Machine::new(program, program);
    let Ok(first) = machine.value(0) else {
        return Want::Internal;
    };
    let Ok(second) = machine.value(0) else {
        return Want::Internal;
    };
    match first {
        M::Str(bytes) => Want::Bytes(bytes),
        M::Table(t) => {
            let mut bytes = Vec::new();
            for item in machine.sequence(t) {
                match integer(item).and_then(|n| u8::try_from(n).ok()) {
                    Some(byte) => bytes.push(byte),
                    None => return Want::Internal,
                }
            }
            Want::Bytes(bytes)
        }
        M::Nil => match second {
            M::Table(t) => {
                // A key that is not a string reads as empty text.
                let text = |name: &str| match machine.field(t, name) {
                    M::Str(s) => String::from_utf8_lossy(&s).into_owned(),
                    _ => String::new(),
                };
                match text("code").as_str() {
                    "unknown_command" => Want::UnknownCommand(text("command")),
                    "missing_field" => Want::MissingField(text("field")),
                    "bad_field" => Want::BadField(text("field"), text("reason")),
                    _ => Want::Internal,
                }
            }
            _ => Want::Internal,
        },
        _ => Want::Internal,
    }
}

/// Mode 2: `encode` of `echo` returns the request as the plugin saw it.
fn encode_request(json: &[u8]) {
    let Some(mut request) = codec::encode_request(json, "echo") else {
        return;
    };
    request.command = "echo".to_owned();
    let got = new_codec().encode(&request);
    let want = if request.fields.values().all(|v| nests_ok(v, 0)) {
        let mut out = Vec::new();
        canonical_object(request.fields.iter(), request.fields.len(), &mut out);
        Want::Bytes(out)
    } else {
        Want::Internal
    };
    check_encode(got, &want, &format!("{:?}", request.fields));
}

/// Whether a JSON value, and what is in it, is at most [`MAX_NESTING`] containers deep
/// (the value itself is at `depth`).
fn nests_ok(value: &Json, depth: usize) -> bool {
    if depth > MAX_NESTING {
        return false;
    }
    match value {
        Json::Array(items) => items.iter().all(|item| nests_ok(item, depth + 1)),
        Json::Object(map) => map.values().all(|item| nests_ok(item, depth + 1)),
        _ => true,
    }
}

/// The string `serialize` in the plugin writes for a value (see there).
fn canonical(value: &Json, out: &mut Vec<u8>) {
    match value {
        Json::Null => out.push(b'n'),
        Json::Bool(true) => out.push(b't'),
        Json::Bool(false) => out.push(b'F'),
        Json::Number(n) => match n.as_i64() {
            Some(i) => out.extend(format!("i{i}").bytes()),
            None => {
                let x = n.as_f64().unwrap_or(f64::NAN);
                out.push(b'f');
                out.extend(encode_hex(&x.to_le_bytes(), "").bytes());
            }
        },
        Json::String(s) => push_string(s, out),
        Json::Array(items) => {
            out.extend(format!("[{}:", items.len()).bytes());
            items.iter().for_each(|item| canonical(item, out));
            out.push(b']');
        }
        Json::Object(map) => canonical_object(map.iter(), map.len(), out),
    }
}

fn canonical_object<'a>(
    entries: impl Iterator<Item = (&'a String, &'a Json)>,
    len: usize,
    out: &mut Vec<u8>,
) {
    // serde_json's map keeps its keys sorted by their bytes, as the plugin sorts them.
    out.extend(format!("{{{len}:").bytes());
    for (key, value) in entries {
        push_string(key, out);
        canonical(value, out);
    }
    out.push(b'}');
}

fn push_string(s: &str, out: &mut Vec<u8>) {
    out.extend(format!("s{}:", s.len()).bytes());
    out.extend(s.bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_replay() {
        crate::replay_seeds("lua_values", super::run);
    }

    /// The model's schema is what the plugin describes.
    #[test]
    fn the_model_schema_is_what_describe_says() {
        let info = new_codec().describe();
        assert_eq!(info.kinds.len(), SCHEMA.len());
        for (kind, (name, declared)) in info.kinds.iter().zip(&SCHEMA) {
            assert_eq!(kind.kind, *name);
            let fields: Vec<_> = kind
                .fields
                .iter()
                .map(|f| (f.name.as_str(), f.ty, f.optional))
                .collect();
            assert_eq!(fields, *declared, "{name}");
        }
        let commands: Vec<_> = info.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(commands, ["run", "echo"]);
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// `array 1 (frame (name plain) (int 1) size nil nil nil)`, then `nil` to hold back.
    const VALID: &[u8] = b"\x09\x01\x11\x08\x00\x03\x01\x10\x00\x00\x00\x00";

    /// Decode `program` as one chunk, through the contract checks.
    fn decode_one(program: &[u8]) -> Vec<Frame> {
        codec::decode_chunks(&mut new_codec(), [program], Instant::now())
    }

    /// Facts the model does not decide: a valid frame comes out whole, and what the
    /// declared fields turn into, written out by hand.
    #[test]
    fn a_valid_frame_comes_out_with_its_fields_converted() {
        let frames = decode_one(VALID);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, "plain");
        assert_eq!(frames[0].raw, 0..VALID.len() as u64);
        assert_eq!(frames[0].severity, Severity::Info);
        assert!(frames[0].fields.is_empty() && frames[0].summary.is_empty());

        // typed: b true, i -3, u 7, f 3.0 (a whole float), s "hi", y 0xFF, l [1, "x"],
        // and x "info", which no kind declares.
        let fields = [
            &b"\x0a\x08"[..],
            b"\x08\x08\x02",
            b"\x08\x09\x03\xfd",
            b"\x08\x0a\x03\x07",
            b"\x08\x0b\x06\x09",
            b"\x08\x0c\x07\x02hi",
            b"\x08\x0d\x07\x01\xff",
            b"\x08\x0e\x09\x02\x03\x01\x07\x01x",
            b"\x08\x11\x08\x04",
        ]
        .concat();
        let program = cat(&[b"\x09\x01\x11\x08\x01\x03\x01\x10\x00\x00", &fields]);
        let frames = decode_one(&program);
        assert_eq!(frames.len(), 1, "{frames:?}");
        let want = [
            ("b", Value::Bool(true)),
            ("i", Value::Int(-3)),
            ("u", Value::UInt(7)),
            ("f", Value::Float(3.0)),
            ("s", Value::Str("hi".into())),
            ("y", Value::Bytes(vec![0xff])),
            (
                "l",
                Value::List(vec![Value::Int(1), Value::Str("x".into())]),
            ),
            ("x", Value::Str("info".into())),
        ];
        let got: Vec<_> = frames[0]
            .fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();
        assert_eq!(got, want);
        // The model agrees, and so does the whole target.
        run(&cat(&[b"\x00\x00", &program]));
    }

    /// Thirteen lists of thirteen lists, twelve times over, built from one table: 40
    /// bytes of program, 13^12 values to convert. The adapter stops it.
    #[test]
    fn a_shared_table_is_not_converted_without_end() {
        let program = cat(&[
            b"\x09\x01\x11\x08\x00\x03\x01\x10\x00\x00",
            b"\x0a\x01\x08\x11",
            &b"\x0b\x0d".repeat(12),
            b"\x03\x01",
        ]);
        let started = Instant::now();
        let frames = decode_one(&program);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, PLUGIN_ERROR_KIND);
        let error = frames[0].field("error").and_then(Value::as_str);
        assert!(error.unwrap().contains("hold more than"), "{error:?}");
        assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
        run(&cat(&[b"\x00\x00", &program]));
    }

    /// The deviation in the module docs. When this fails the adapter keeps the order, and
    /// `decode`'s use of `decode_unchecked` can go.
    #[test]
    fn the_adapter_still_breaks_the_frame_order_in_two_ways() {
        let t0 = Instant::now();
        let frame = |pos: u8| cat(&[b"\x11\x08\x00\x03", &[pos], b"\x03\x02\x00\x00\x00"]);
        // A plugin's frames out of order are passed on as they are.
        let program = cat(&[b"\x09\x02", &frame(9), &frame(1)]);
        let frames = decode_unchecked(&mut new_codec(), &[&program], t0);
        assert_eq!(
            frames.iter().map(|f| f.raw.start).collect::<Vec<_>>(),
            [8, 0]
        );
        // A list that goes bad after a frame fails the whole call, after that frame.
        let program = cat(&[b"\x09\x02", &frame(3), b"\x03\x05"]);
        let frames = decode_unchecked(&mut new_codec(), &[&program], t0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].raw.start, 2);
        assert_eq!(frames[1].kind, PLUGIN_ERROR_KIND);
        assert_eq!(frames[1].raw, 0..program.len() as u64);
    }
}
