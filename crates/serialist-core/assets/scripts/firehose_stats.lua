-- firehose_stats.lua: count the lines that arrive in two seconds and print the rate.
--
-- Try it against the simulator's firehose (`serialist --virtual firehose`). The count
-- comes from an on_line callback, which runs for every received line while the main
-- body sleeps.

local SECONDS = 2
local port = assert(serial.current())
local lines, bytes = 0, 0
local counter <close> = port:on_line(function(line)
  lines = lines + 1
  bytes = bytes + #line
end)
sleep(SECONDS * 1000)
counter:cancel()
print(string.format("%d lines in %.1f s: %.0f lines/s, %.1f KiB/s of text",
  lines, SECONDS, lines / SECONDS, bytes / SECONDS / 1024))
