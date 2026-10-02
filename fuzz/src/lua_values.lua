-- The plugin behind fuzz/src/lua_values.rs: it returns whatever its input bytes say, so
-- the fuzzer decides which Lua values cross into serialist-plugins' adapter. The input
-- format, the opcodes and what the Rust side expects are documented there; the two must
-- agree on every number below (NAMES, EDGES, SPECIALS, the opcode table, MAX_DEPTH).
--
-- decode(bytes): the bytes are a program. It builds two values, the frames and the
-- bytes to hold back, and returns them as they are.
-- encode({command = "run", fields = {program = <hex>}}): the same, for encode's two
--   results (bytes or nil, then an error).
-- encode({command = "echo", fields = ...}): the request as Lua received it, written
--   out as a canonical string, to check what the adapter hands a plugin.

local MAX_DEPTH = 48

-- Names the `name` opcode picks from: frame kinds (the first three are declared by
-- describe, "other" is not), severities, declared field names, a few undeclared ones,
-- the keys of a frame, and the keys of an error table `encode` may return.
local NAMES = {
  "plain", "typed", "loose", "other", "info", "warning", "error", "fatal",
  "b", "i", "u", "f", "s", "y", "l", "o",
  "a", "x", "kind", "pos", "len", "severity", "summary", "fields",
  "code", "unknown_command", "missing_field", "bad_field", "command", "field", "reason", "",
}

-- Integers at the edges of what a frame position or a field can hold.
local EDGES = {
  math.maxinteger, math.mininteger, math.maxinteger - 1, math.mininteger + 1,
  1 << 31, -(1 << 31), 1 << 53, 255,
}

-- Floats the adapter must not trip over.
local SPECIALS = {
  0 / 0, math.huge, -math.huge, -0.0,
  2.0 ^ 63, -(2.0 ^ 63), 2.0 ^ 53, 0.5,
  -1.0, 3.0, 1e300, 255.0,
  256.0, 2.0 ^ 63 - 1024.0, 4294967296.0, 1.5,
}

-- A machine that reads `program` as a bytecode and builds one value per `value` call.
-- `input` is what `size` and `tail` read from. Reading past the end gives 0 bytes, so
-- every program is valid and a short one ends in nils.
local function machine(program, input)
  local pos, env = 1, {}

  local function u8()
    local b = program:byte(pos) or 0
    pos = pos + 1
    return b
  end

  local function i8()
    local b = u8()
    if b >= 128 then b = b - 256 end
    return b
  end

  local function f64()
    local b = {}
    for i = 1, 8 do b[i] = u8() end
    return (string.unpack("<d", string.char(table.unpack(b))))
  end

  local value

  -- Fill `t` from `n` key and value pairs. A nil or NaN key raises, as in any Lua.
  local function entries(t, n, depth)
    for _ = 1, n do
      local k = value(depth + 1)
      local v = value(depth + 1)
      t[k] = v
    end
  end

  function value(depth)
    if depth >= MAX_DEPTH then return nil end
    local op = u8() % 20
    if op == 0 then
      return nil
    elseif op == 1 then
      return false
    elseif op == 2 then
      return true
    elseif op == 3 then
      return i8()
    elseif op == 4 then
      return EDGES[u8() % #EDGES + 1]
    elseif op == 5 then
      return f64()
    elseif op == 6 then
      return SPECIALS[u8() % #SPECIALS + 1]
    elseif op == 7 then
      local n = u8()
      local s = program:sub(pos, pos + n - 1)
      pos = pos + n
      return s
    elseif op == 8 then
      return NAMES[u8() % #NAMES + 1]
    elseif op == 9 then
      local t = {}
      for i = 1, u8() % 17 do t[i] = value(depth + 1) end
      return t
    elseif op == 10 then
      local t = {}
      entries(t, u8() % 9, depth)
      return t
    elseif op == 11 then
      -- The same value n times: a table that is a graph, not a tree, so what is
      -- converted can be far bigger than what was built.
      local n = u8() % 17
      local v = value(depth + 1)
      local t = {}
      for i = 1, n do t[i] = v end
      return t
    elseif op == 12 then
      -- A record that later parts of the program can refer to, itself included.
      local t = {}
      env[#env + 1] = t
      entries(t, u8() % 9, depth)
      return t
    elseif op == 13 then
      local k = u8()
      if #env == 0 then return nil end
      return env[k % #env + 1]
    elseif op == 14 then
      return function() end
    elseif op == 15 then
      return coroutine.create(function() end)
    elseif op == 16 then
      return #input
    elseif op == 17 then
      local kind = value(depth + 1)
      local p = value(depth + 1)
      local l = value(depth + 1)
      local severity = value(depth + 1)
      local summary = value(depth + 1)
      local fields = value(depth + 1)
      return {
        kind = kind, pos = p, len = l, severity = severity, summary = summary,
        fields = fields,
      }
    elseif op == 18 then
      return input:sub(math.max(1, #input - u8() + 1))
    else
      local k = u8() % 3
      if k == 0 then error("raised by the program") end
      if k == 1 then error({}) end
      error(nil)
    end
  end

  return value
end

-- The request, written as a string both sides can build: one letter for the type, then
-- the value. Objects list their keys in byte order; floats are their 8 bytes in hex.
local function serialize(v, out)
  local t = type(v)
  if v == codec.null then
    out[#out + 1] = "n"
  elseif t == "boolean" then
    out[#out + 1] = v and "t" or "F"
  elseif t == "number" then
    if math.type(v) == "integer" then
      out[#out + 1] = "i" .. v
    else
      out[#out + 1] = "f" .. hex.encode(string.pack("<d", v), "")
    end
  elseif t == "string" then
    out[#out + 1] = "s" .. #v .. ":" .. v
  elseif t == "table" and codec.is_array(v) then
    out[#out + 1] = "[" .. #v .. ":"
    for i = 1, #v do serialize(v[i], out) end
    out[#out + 1] = "]"
  elseif t == "table" then
    local keys = {}
    for k in pairs(v) do keys[#keys + 1] = k end
    table.sort(keys)
    out[#out + 1] = "{" .. #keys .. ":"
    for _, k in ipairs(keys) do
      serialize(k, out)
      serialize(v[k], out)
    end
    out[#out + 1] = "}"
  else
    error("cannot serialize a " .. t)
  end
end

local M = {}

function M.describe()
  return {
    name = "lua-values", version = "0.0.0", description = "returns what its input says",
    kinds = {
      { kind = "plain", description = "declares no fields" },
      { kind = "typed", description = "one declared field of each type", fields = {
          { name = "b", type = "bool" },
          { name = "i", type = "int" },
          { name = "u", type = "uint" },
          { name = "f", type = "float" },
          { name = "s", type = "str" },
          { name = "y", type = "bytes" },
          { name = "l", type = "list" },
          { name = "o", type = "uint", optional = true },
      } },
      { kind = "loose", description = "only optional fields", fields = {
          { name = "a", type = "int", optional = true },
          { name = "s", type = "str", optional = true },
      } },
    },
    commands = {
      { name = "run", description = "encode returns what the program says", fields = {
          { name = "program", type = "str" },
      } },
      { name = "echo", description = "encode returns its request, serialized" },
    },
  }
end

function M.decode(bytes, _state)
  local value = machine(bytes, bytes)
  local frames = value(0)
  local rest = value(0)
  return frames, rest
end

function M.encode(request)
  local fields = request.fields
  if request.command == "run" then
    local program = hex.decode(fields.program)
    local value = machine(program, program)
    local first = value(0)
    local second = value(0)
    return first, second
  elseif request.command == "echo" then
    local out = {}
    serialize(fields, out)
    return table.concat(out)
  end
  return nil, codec.unknown_command(request.command)
end

return M
