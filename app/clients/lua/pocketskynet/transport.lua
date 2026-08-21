-- transport.lua — one HTTP transport for both HTTP/1.1 and HTTP/3.
--
-- Neither luarocks nor an HTTP-capable Lua library is guaranteed on the
-- target machine, and the system libcurl on macOS (SecureTransport build)
-- has no HTTP/3 — so the pragmatic single code path is the `curl` binary:
-- the same invocation serves both protocols, and `http3 = true` simply adds
-- curl's `--http3` flag (which requires an HTTP/3-enabled curl, e.g.
-- Homebrew's; see the README). `insecure = true` maps to `-k` for the
-- server's self-signed development certificates.
--
-- Request bodies travel via a temp file (`--data-binary @file`), never
-- through shell interpolation, and every argument is single-quoted.

local M = {}
M.__index = M

local function shquote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

--- opts: server (base URL), http3 (bool), insecure (bool), curl (binary path)
function M.new(opts)
  opts = opts or {}
  return setmetatable({
    server = (opts.server or "http://127.0.0.1:9081"):gsub("/+$", ""),
    http3 = opts.http3 or false,
    insecure = opts.insecure or false,
    curl = opts.curl or os.getenv("POCKETSKYNET_CURL") or "curl",
  }, M)
end

--- Perform a request. body is a raw string (already JSON-encoded) or nil.
--- headers is a { name = value } table or nil.
--- Returns status (number), body (string) — or nil, error-message.
function M:request(method, path, body, headers)
  local args = { self.curl, "-sS", "--max-time", "30", "-X", method }
  if self.http3 then
    args[#args + 1] = "--http3"
  end
  if self.insecure then
    args[#args + 1] = "-k"
  end
  for name, value in pairs(headers or {}) do
    args[#args + 1] = "-H"
    args[#args + 1] = name .. ": " .. value
  end

  local body_file
  if body then
    body_file = os.tmpname()
    local f = assert(io.open(body_file, "wb"))
    f:write(body)
    f:close()
    args[#args + 1] = "-H"
    args[#args + 1] = "Content-Type: application/json"
    args[#args + 1] = "--data-binary"
    args[#args + 1] = "@" .. body_file
  end

  -- curl expands \n itself, so the status code lands on its own line.
  args[#args + 1] = "-w"
  args[#args + 1] = "\\n%{http_code}"
  args[#args + 1] = self.server .. path

  local err_file = os.tmpname()
  local parts = {}
  for i, a in ipairs(args) do parts[i] = shquote(a) end
  local cmd = table.concat(parts, " ") .. " 2> " .. shquote(err_file)

  local pipe = assert(io.popen(cmd, "r"))
  local out = pipe:read("a") or ""
  local ok, _, code = pipe:close()

  local stderr = ""
  local ef = io.open(err_file, "rb")
  if ef then
    stderr = ef:read("a") or ""
    ef:close()
  end
  os.remove(err_file)
  if body_file then os.remove(body_file) end

  if not ok then
    stderr = stderr:gsub("%s+$", "")
    if stderr == "" then stderr = "no error output" end
    return nil, string.format("curl failed (exit %s): %s", tostring(code), stderr)
  end

  local resp_body, status = out:match("^(.*)\n(%d+)%s*$")
  if not status then
    return nil, "could not parse curl output: " .. out
  end
  return tonumber(status), resp_body
end

-- Exposed for the test suite: the quoting function is security-relevant
-- (hostile message text must stay an inert argument) and tested directly.
M._shquote = shquote

return M
