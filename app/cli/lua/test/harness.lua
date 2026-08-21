-- harness.lua — boots a real `pocketskynet` process per integration test
-- group, mirroring the spirit of app/server/tests/common/harness.rs.
--
-- Every server gets its own port and its own temp data directory. The Rust
-- harness serialises boots behind a lock because a "pick a free port, hand
-- it to a child" dance can hand two children the same port — and the loser's
-- health probe is then answered by the winner, so a test quietly drives
-- somebody else's server. This Lua suite runs tests sequentially in one
-- process, so there is no cross-test race to lock against, but the same
-- verification is kept: after /api/health answers, the child must still be
-- alive (a child that lost a bind race has already exited), otherwise the
-- boot is retried on a fresh port.
--
-- Teardown: every spawned pid is tracked in M.servers; M.stop_all() kills
-- them all and is safe to call twice. The integration suite registers it as
-- a runner at-exit hook, so even a failing test (or an os.exit on the
-- failure path) cannot leak a server.

local M = { servers = {} }

--- Shared with the server via --jwt-secret, so tests could mint tokens.
M.JWT_SECRET = "pocketskynet-lua-integration-test-secret-0123456789abcdef"

local BOOT_TIMEOUT_S = 30

local function shquote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

local test_dir = debug.getinfo(1, "S").source:match("^@(.*)[/\\][^/\\]*$") or "."
local app_dir = test_dir .. "/../../.."

local function is_file(path)
  local f = io.open(path, "rb")
  if f then
    f:close()
    return true
  end
  return false
end

local function popen_line(cmd)
  local p = io.popen(cmd, "r")
  if not p then return nil end
  local line = p:read("l")
  p:close()
  return line
end

--- Locate the pocketskynet server binary.
--- Order: $POCKETSKYNET_BIN, this checkout's app/target/{release,debug},
--- then — because git worktrees do not share an untracked target/ dir —
--- the main checkout's app/target/{release,debug} via git-common-dir.
--- Returns path or nil, error-message.
function M.find_binary()
  local candidates = {}
  local env = os.getenv("POCKETSKYNET_BIN")
  if env and env ~= "" then candidates[#candidates + 1] = env end
  candidates[#candidates + 1] = app_dir .. "/target/release/pocketskynet"
  candidates[#candidates + 1] = app_dir .. "/target/debug/pocketskynet"
  -- A linked git worktree does not share the main checkout's untracked
  -- target/ directory, so also look for the binary in the main checkout.
  local common = popen_line("git -C " .. shquote(test_dir) ..
    " rev-parse --path-format=absolute --git-common-dir 2>/dev/null")
  if common and common ~= "" then
    -- ordinary repository: <root>/.git
    local root = common:match("^(.*)/%.git$")
    if root then
      candidates[#candidates + 1] = root .. "/app/target/release/pocketskynet"
      candidates[#candidates + 1] = root .. "/app/target/debug/pocketskynet"
    end
    -- absorbed submodule: the gitdir lives under the superproject's
    -- .git/modules and records the checkout in core.worktree (relative)
    local rel = popen_line("git --git-dir " .. shquote(common) ..
      " config --get core.worktree 2>/dev/null")
    if rel and rel ~= "" then
      local resolved = popen_line("cd " .. shquote(common) .. " 2>/dev/null && cd " ..
        shquote(rel) .. " 2>/dev/null && pwd -P")
      if resolved and resolved ~= "" then
        candidates[#candidates + 1] = resolved .. "/app/target/release/pocketskynet"
        candidates[#candidates + 1] = resolved .. "/app/target/debug/pocketskynet"
      end
    end
  end
  for _, path in ipairs(candidates) do
    if is_file(path) then return path end
  end
  return nil, "pocketskynet server binary not found. Build it first\n" ..
    "  (cd app && cargo build --release -p pocketskynet-server)\n" ..
    "or point POCKETSKYNET_BIN at an existing binary. Tried:\n  " ..
    table.concat(candidates, "\n  ")
end

local function port_in_use(port)
  -- nc exits 0 when something is listening
  return os.execute("nc -z -w 1 127.0.0.1 " .. port .. " >/dev/null 2>&1") == true
end

local function pick_port()
  for _ = 1, 50 do
    local port = math.random(20000, 59999)
    if not port_in_use(port) then return port end
  end
  error("could not find a free port after 50 tries")
end

local function pid_alive(pid)
  return os.execute("kill -0 " .. pid .. " 2>/dev/null") == true
end

local function now()
  return os.time()
end

local counter = 0
local function unique_dir()
  counter = counter + 1
  local base = os.getenv("TMPDIR") or "/tmp"
  return string.format("%s/ps-lua-it-%d-%d-%d",
    base:gsub("/+$", ""), os.time(), math.random(1e6), counter)
end

local function log_tail(path, lines)
  local f = io.open(path, "rb")
  if not f then return "(no server log)" end
  local content = f:read("a") or ""
  f:close()
  local all = {}
  for line in content:gmatch("[^\n]+") do all[#all + 1] = line end
  local from = math.max(1, #all - (lines or 25) + 1)
  return table.concat(all, "\n", from)
end

--- Start a server. opts:
---   tls   = true  → serve HTTPS (self-signed; clients use --insecure)
---   http3 = true  → also serve HTTP/3 on the SAME port number over UDP
---                   (the conventional deployment; TCP and UDP namespaces
---                   are separate, per app/server/src/config.rs)
---   args  = { ... } extra CLI flags
--- Returns a server table: { pid, port, base_url, data_dir, log_path,
--- scheme, stop() }. Raises on failure with the server log tail.
function M.start(opts)
  opts = opts or {}
  local bin, err = M.find_binary()
  if not bin then error(err, 0) end

  local last_err = "unknown"
  for _ = 1, 3 do
    local server, boot_err = M.try_start(bin, opts)
    if server then return server end
    last_err = boot_err
  end
  error("could not start pocketskynet after 3 attempts: " .. last_err, 0)
end

function M.try_start(bin, opts)
  local port = pick_port()
  local data_dir = unique_dir()
  local static_dir = data_dir .. "/static"
  os.execute("mkdir -p " .. shquote(static_dir))
  local log_path = data_dir .. "/server.log"

  local args = {
    bin,
    "--host", "127.0.0.1",
    "--port", tostring(port),
    "--data-dir", data_dir,
    "--static-dir", static_dir,
    "--jwt-secret", M.JWT_SECRET,
    "--no-rate-limit",
    "--no-payment-verify",
    "--no-mdns",
    "--log", "warn",
  }
  if opts.tls then args[#args + 1] = "--tls" end
  if opts.http3 then
    args[#args + 1] = "--http3"
    args[#args + 1] = "--http3-port"
    args[#args + 1] = tostring(port)
  end
  for _, a in ipairs(opts.args or {}) do args[#args + 1] = a end

  local quoted = {}
  for i, a in ipairs(args) do quoted[i] = shquote(a) end
  -- Inherited PS_* variables must not decide what the suite tests.
  local cleanup_env = "unset PS_HOST PS_PORT POCKETSKYNET_PATH PS_STATIC_DIR " ..
    "PS_JWT_SECRET PS_TLS PS_HTTP3 PS_HTTP3_PORT PS_NO_RATE_LIMIT PS_LOG; " ..
    "export PS_IGNORE_BAKED_ENV=1; "
  local cmd = cleanup_env .. table.concat(quoted, " ") ..
    " > " .. shquote(log_path) .. " 2>&1 & echo $!"
  local pid = tonumber(popen_line(cmd))
  if not pid then
    return nil, "could not spawn server (no pid)"
  end

  local scheme = opts.tls and "https" or "http"
  local server = {
    pid = pid,
    port = port,
    scheme = scheme,
    base_url = scheme .. "://127.0.0.1:" .. port,
    data_dir = data_dir,
    log_path = log_path,
  }
  server.stop = function() M.stop(server) end
  M.servers[#M.servers + 1] = server

  local health = server.base_url .. "/api/health"
  local deadline = now() + BOOT_TIMEOUT_S
  while now() < deadline do
    if not pid_alive(pid) then
      M.stop(server)
      return nil, "server exited during boot\n--- log tail ---\n" .. log_tail(log_path)
    end
    local body = popen_line("curl -sk --max-time 2 " .. shquote(health) .. " 2>/dev/null")
    if body and body:find('"status":"ok"', 1, true) then
      -- Somebody answered — make sure it was our child, not the winner of
      -- a bind race our child lost.
      if pid_alive(pid) then
        return server
      end
      M.stop(server)
      return nil, "another process owns port " .. port .. "; our child exited"
    end
    os.execute("sleep 0.1")
  end
  M.stop(server)
  return nil, "/api/health never became ready on port " .. port ..
    "\n--- log tail ---\n" .. log_tail(log_path)
end

--- Kill one server and remove its data directory. Idempotent.
function M.stop(server)
  if server.stopped then return end
  server.stopped = true
  os.execute("kill -9 " .. server.pid .. " 2>/dev/null")
  -- wait for it to actually die so the port is really free again
  for _ = 1, 20 do
    if not pid_alive(server.pid) then break end
    os.execute("sleep 0.1")
  end
  os.execute("rm -rf " .. shquote(server.data_dir))
end

--- Kill every server this process ever started. Safe to call repeatedly.
function M.stop_all()
  for _, server in ipairs(M.servers) do
    M.stop(server)
  end
end

--- True when no tracked server process is still alive (leak check).
function M.none_alive()
  for _, server in ipairs(M.servers) do
    if pid_alive(server.pid) then return false end
  end
  return true
end

math.randomseed(os.time() + (tonumber(tostring({}):match("0x(%x+)"), 16) or 0) % 100000)

return M
