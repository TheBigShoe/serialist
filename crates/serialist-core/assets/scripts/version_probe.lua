-- version_probe.lua: check that an AT device answers, then ask for its version.
--
-- Run it from the Script console, a key binding or a saved command against the
-- simulator's AT modem (`serialist --virtual at`), or headless:
-- `serialist --port virtual:at --script version_probe.lua`.

local port = assert(serial.current())  -- or serial.open{ match = { product = "Airoha" }, baud = 921600 }
port:write("AT\r\n")
local ok = port:expect("^OK$", { timeout_ms = 1000 })
assert(ok, "no OK from the device")
log.info("matched", ok[1])
for i = 1, 3 do
  port:write("AT+VER?\r\n")
  local m = port:expect([[^\+VER: (\S+)]], { timeout_ms = 500 })
  log.info("value", i, m and m[2] or "timeout")
  sleep(100)
end
