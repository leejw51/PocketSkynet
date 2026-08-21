-- unit_cli.lua — the CLI's argument handling and exit codes, exercised by
-- actually running pocketskynet.lua as a child process (no server; the one
-- networked case points at a closed port and asserts the failure path).

local test_dir = debug.getinfo(1, "S").source:match("^@(.*)[/\\][^/\\]*$") or "."
local CLI = test_dir .. "/../pocketskynet.lua"

local function shquote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

--- Run the CLI; returns exit code, combined stdout+stderr.
local function run_cli(args)
  local parts = { "lua", shquote(CLI) }
  for _, a in ipairs(args) do parts[#parts + 1] = shquote(a) end
  -- a clean environment: ambient credentials must not change CLI behavior
  local cmd = "env -u POCKETSKYNET_KEY -u POCKETSKYNET_TOKEN " ..
    "-u POCKETSKYNET_SERVER -u POCKETSKYNET_CURL " ..
    table.concat(parts, " ") .. " 2>&1"
  local p = assert(io.popen(cmd, "r"))
  local out = p:read("a") or ""
  local ok, _, code = p:close()
  if ok then code = 0 end
  return code, out
end

return function(T)
  T.test("cli with no arguments prints usage and exits 1", function()
    local code, out = run_cli({})
    T.eq(code, 1)
    T.contains(out, "Usage:")
  end)

  T.test("cli --help exits 0", function()
    local code, out = run_cli({ "--help" })
    T.eq(code, 0)
    T.contains(out, "Usage:")
    T.contains(out, "--http3")
  end)

  T.test("cli rejects an unknown flag with exit 1", function()
    local code, out = run_cli({ "--frobnicate", "health" })
    T.eq(code, 1)
    T.contains(out, "unknown flag --frobnicate")
  end)

  T.test("cli rejects an unknown command with exit 1", function()
    local code, out = run_cli({ "frobnicate" })
    T.eq(code, 1)
    T.contains(out, "unknown command 'frobnicate'")
  end)

  T.test("cli rejects a value flag without a value", function()
    local code, out = run_cli({ "health", "--server" })
    T.eq(code, 1)
    T.contains(out, "--server requires a value")
  end)

  T.test("cli login without a key exits 1 with guidance", function()
    local code, out = run_cli({ "login" })
    T.eq(code, 1)
    T.contains(out, "POCKETSKYNET_KEY")
  end)

  T.test("cli send without arguments exits 1 with usage", function()
    local code, out = run_cli({ "send" })
    T.eq(code, 1)
    T.contains(out, "usage: send <roomId> <text>")
  end)

  T.test("cli messages without a room id exits 1", function()
    local code, out = run_cli({ "messages" })
    T.eq(code, 1)
    T.contains(out, "usage: messages <roomId>")
  end)

  T.test("cli create-room without a name exits 1", function()
    local code, out = run_cli({ "create-room" })
    T.eq(code, 1)
    T.contains(out, "usage: create-room <name>")
  end)

  T.test("cli health against a dead server exits 1 with a curl error", function()
    -- port 9 (discard) on localhost is refused on macOS dev machines
    local code, out = run_cli({ "--server", "http://127.0.0.1:9", "health" })
    T.eq(code, 1)
    T.contains(out, "error:")
  end)

  T.test("cli rejects a malformed private key before any network use", function()
    local code, out = run_cli({
      "--server", "http://127.0.0.1:9", "--key", "0xnothex", "login",
    })
    T.eq(code, 1)
    T.contains(out, "64 hex")
  end)
end
