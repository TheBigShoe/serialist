-- Airoha RACE: the reference codec plugin (tier 1, Lua).
--
-- It must agree byte for byte with the Rust codec in serialist-plugins/src/race.rs: the
-- same describe(), the same frames for any way the stream is cut into chunks, and the
-- same bytes or error for every encode request. tests/conformance.rs checks all three.
--
-- Wire format: [0x05][type: u8][len: u16 LE][cmd_id: u16 LE][payload], len = #payload + 2.
-- Types: 0x5A command, 0x5B response, 0x5C indication, 0x5D log.
--
-- Decoding is a resynchronising framer. A 0x05 followed by a known type and a length of
-- 2..4096 starts a frame. A known type with any other length is a "malformed" frame of
-- the sync and type bytes, and scanning resumes at the length bytes. Everything else is
-- text, cut at each line feed (included) or every 1024 bytes. Bytes that are not decided
-- yet (a partial frame, or a text line with no line feed) are returned as the remainder,
-- which the host prepends to the next chunk.

local SYNC = 0x05
local LF = 0x0A
local CR = 0x0D
local MAX_LEN = 4096
local MAX_PAYLOAD = MAX_LEN - 2
local MAX_TEXT = 1024
local PREVIEW = 16
local VERSION_CMD_ID = 0x0F15

local KIND = { [0x5A] = "command", [0x5B] = "response", [0x5C] = "indication", [0x5D] = "log" }
local TYPE = { command = 0x5A, response = 0x5B, indication = 0x5C, log = 0x5D }

local byte, char, sub, find, format = string.byte, string.char, string.sub, string.find, string.format
local pack, unpack = string.pack, string.unpack

local M = {}

-- describe --------------------------------------------------------------------------

local function frame_kind(kind, description)
  return {
    kind = kind,
    description = description,
    fields = {
      { name = "type", type = "uint", description = "The type byte" },
      { name = "cmd_id", type = "uint", description = "The command id" },
      { name = "cmd_id_hex", type = "str", description = "The command id as 0xNNNN" },
      { name = "payload", type = "bytes", description = "The bytes after the command id" },
      { name = "payload_len", type = "uint", description = "Payload length in bytes" },
    },
  }
end

function M.describe()
  return {
    name = "airoha-race",
    version = "1.0.0",
    description = "Airoha RACE: 0x05-framed commands, responses, indications and logs, with the text between frames",
    kinds = {
      frame_kind("command", "A command to the device (type 0x5A)"),
      frame_kind("response", "A response from the device (type 0x5B)"),
      frame_kind("indication", "An unsolicited indication (type 0x5C)"),
      frame_kind("log", "Log data (type 0x5D)"),
      {
        kind = "malformed",
        description = "A sync byte and a known type followed by an impossible length",
        fields = {
          { name = "type", type = "uint", description = "The type byte" },
          { name = "len", type = "uint", description = "The length field as received" },
          { name = "reason", type = "str", description = "Why the header was rejected" },
        },
      },
      {
        kind = "text",
        description = "Bytes between frames, a line or 1024 bytes at a time",
        fields = {
          {
            name = "text",
            type = "str",
            description = "Printable ASCII as is, other bytes escaped, the line ending dropped",
          },
        },
      },
    },
    commands = {
      {
        name = "race",
        description = "Any RACE frame",
        fields = {
          {
            name = "type",
            type = "str",
            optional = true,
            description = "command, response, indication or log, or a type byte; default command",
          },
          {
            name = "cmd_id",
            type = "uint",
            description = "The command id: an integer or hex such as 0x0F15",
          },
          {
            name = "payload",
            type = "bytes",
            optional = true,
            description = "Hex text or a list of bytes; default empty",
          },
        },
      },
      {
        name = "race_version",
        description = "Query version and build time (command 0x0F15, no payload)",
        fields = {},
      },
    },
  }
end

-- decode ----------------------------------------------------------------------------

-- Text rendering: printable ASCII as is; tab, CR, LF and backslash as \t \r \n \\; any
-- other byte as \xNN.
local ESC = {}
for b = 0, 255 do
  local c = char(b)
  if c == "\\" then
    ESC[c] = "\\\\"
  elseif c == "\t" then
    ESC[c] = "\\t"
  elseif c == "\r" then
    ESC[c] = "\\r"
  elseif c == "\n" then
    ESC[c] = "\\n"
  elseif b < 0x20 or b > 0x7E then
    ESC[c] = format("\\x%02X", b)
  end
end

local function render(s)
  return (s:gsub("[%c\\\128-\255]", ESC))
end

local function text_frame(frames, bytes, first, last)
  local s = sub(bytes, first, last)
  if byte(s, -1) == LF then
    s = sub(s, 1, -2)
    if byte(s, -1) == CR then
      s = sub(s, 1, -2)
    end
  end
  local text = render(s)
  frames[#frames + 1] = {
    kind = "text",
    pos = first,
    len = last - first + 1,
    summary = text,
    fields = { text = text },
  }
end

local function summary(kind, cmd_id, payload)
  local s = format("%s 0x%04X len %d", kind, cmd_id, #payload)
  if #payload > 0 then
    s = s .. ": " .. hex.encode(sub(payload, 1, PREVIEW), " ")
    if #payload > PREVIEW then
      s = s .. " ..."
    end
  end
  return s
end

function M.decode(bytes, state)
  local frames = {}
  local n = #bytes
  local i = 1
  -- The open text run, bytes tstart..tend.
  local tstart, tend = nil, nil

  local function add_text(first, last)
    if tstart == nil then
      tstart = first
    end
    tend = last
    while tend - tstart + 1 >= MAX_TEXT do
      text_frame(frames, bytes, tstart, tstart + MAX_TEXT - 1)
      tstart = tstart + MAX_TEXT
    end
    if tstart > tend then
      tstart, tend = nil, nil
    end
  end

  local function flush_text()
    if tstart ~= nil then
      text_frame(frames, bytes, tstart, tend)
      tstart, tend = nil, nil
    end
  end

  while i <= n do
    if byte(bytes, i) == SYNC then
      if i + 1 > n then
        break -- wait for the type byte
      end
      local ty = byte(bytes, i + 1)
      local kind = KIND[ty]
      if kind == nil then
        add_text(i, i) -- not a frame: the sync byte is text
        i = i + 1
      else
        if i + 3 > n then
          break -- wait for the length
        end
        local len = unpack("<I2", bytes, i + 2)
        if len < 2 or len > MAX_LEN then
          flush_text()
          local reason = format("length %d is outside 2..=%d", len, MAX_LEN)
          frames[#frames + 1] = {
            kind = "malformed",
            pos = i,
            len = 2,
            severity = "warning",
            summary = format("malformed 0x%02X header: %s", ty, reason),
            fields = { type = ty, len = len, reason = reason },
          }
          i = i + 2 -- the length bytes are scanned again
        else
          local last = i + 3 + len
          if last > n then
            break -- wait for the rest of the frame
          end
          flush_text()
          local cmd_id = unpack("<I2", bytes, i + 4)
          local payload = sub(bytes, i + 6, last)
          frames[#frames + 1] = {
            kind = kind,
            pos = i,
            len = last - i + 1,
            summary = summary(kind, cmd_id, payload),
            fields = {
              type = ty,
              cmd_id = cmd_id,
              cmd_id_hex = format("0x%04X", cmd_id),
              payload = payload,
              payload_len = #payload,
            },
          }
          i = last + 1
        end
      end
    else
      local j = find(bytes, "[\5\n]", i)
      if j == nil then
        add_text(i, n)
        i = n + 1
      elseif byte(bytes, j) == LF then
        add_text(i, j)
        flush_text()
        i = j + 1
      else
        add_text(i, j - 1)
        i = j
      end
    end
  end
  return frames, sub(bytes, tstart or i)
end

-- encode ----------------------------------------------------------------------------

-- "0x0F15", "0X0f15" or "0F15" as a number; nil if it is not hex digits. More than 15
-- significant digits is more than any field accepts, so it is refused before
-- tonumber could wrap around.
local function parse_hex_uint(s)
  local digits = s:match("^0[xX](.*)$") or s
  if digits == "" or find(digits, "[^%x]") then
    return nil
  end
  digits = digits:gsub("^0+", "")
  if digits == "" then
    return 0
  end
  if #digits > 15 then
    return nil
  end
  return tonumber(digits, 16)
end

-- Returns the value (nil if absent) or nil and an error.
local function uint_field(fields, name, max)
  local v = fields[name]
  if v == nil then
    return nil
  end
  local n
  if math.type(v) == "integer" then
    if v < 0 then
      return nil, codec.bad_field(name, format("%d is not a non-negative integer", v))
    end
    n = v
  elseif type(v) == "string" then
    n = parse_hex_uint(v)
    if n == nil then
      return nil, codec.bad_field(name, format("%q is not hex digits (with or without 0x)", v))
    end
  else
    return nil, codec.bad_field(name, "must be an integer or a hex string")
  end
  if n > max then
    return nil, codec.bad_field(name, format("0x%X is larger than 0x%X", n, max))
  end
  return n
end

local function bytes_field(fields, name)
  local v = fields[name]
  if v == nil then
    return nil
  end
  if type(v) == "string" then
    local ok, decoded = pcall(hex.decode, v)
    if not ok then
      return nil, codec.bad_field(name, tostring(decoded))
    end
    return decoded
  end
  if codec.is_array(v) then
    local parts = {}
    for i, item in ipairs(v) do
      if math.type(item) ~= "integer" or item < 0 or item > 255 then
        return nil, codec.bad_field(name, format("item %d is not a byte (an integer from 0 to 255)", i - 1))
      end
      parts[i] = char(item)
    end
    return table.concat(parts)
  end
  return nil, codec.bad_field(name, "must be hex text or a list of bytes")
end

local function type_field(fields)
  local v = fields.type
  if v == nil then
    return TYPE.command
  end
  local b
  if type(v) == "string" then
    b = TYPE[v] or parse_hex_uint(v)
  elseif math.type(v) == "integer" then
    b = v
  end
  if b == nil or KIND[b] == nil then
    return nil, codec.bad_field("type", "must be command, response, indication or log, or a byte from 0x5A to 0x5D")
  end
  return b
end

-- The alphabetically first field not in `allowed`, as an error.
local function check_fields(fields, command, allowed)
  local unknown = {}
  for key in pairs(fields) do
    if not allowed[key] then
      unknown[#unknown + 1] = key
    end
  end
  if #unknown > 0 then
    table.sort(unknown)
    return codec.bad_field(unknown[1], format("not a field of `%s`", command))
  end
end

local function frame(ty, cmd_id, payload)
  if #payload > MAX_PAYLOAD then
    return nil, codec.bad_field("payload", format("%d bytes is more than the %d a frame carries", #payload, MAX_PAYLOAD))
  end
  return pack("<BBI2I2", SYNC, ty, #payload + 2, cmd_id) .. payload
end

function M.encode(request)
  local command, fields = request.command, request.fields
  if command == "race" then
    local err = check_fields(fields, command, { type = true, cmd_id = true, payload = true })
    if err then
      return nil, err
    end
    local ty, cmd_id, payload
    ty, err = type_field(fields)
    if err then
      return nil, err
    end
    cmd_id, err = uint_field(fields, "cmd_id", 0xFFFF)
    if err then
      return nil, err
    end
    if cmd_id == nil then
      return nil, codec.missing_field("cmd_id")
    end
    payload, err = bytes_field(fields, "payload")
    if err then
      return nil, err
    end
    return frame(ty, cmd_id, payload or "")
  elseif command == "race_version" then
    local err = check_fields(fields, command, {})
    if err then
      return nil, err
    end
    return frame(TYPE.command, VERSION_CMD_ID, "")
  end
  return nil, codec.unknown_command(command)
end

return M
