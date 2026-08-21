-- unit_transport.lua — the curl transport, driven against a fake `curl`
-- executable that records its argv and the request body. This is where the
-- shell-quoting security properties are pinned: hostile message text,
-- headers, and URLs must arrive as inert single arguments (or body bytes),
-- never as executed shell.

local transport = require("pocketskynet.transport")
local json = require("pocketskynet.json")
local Client = require("pocketskynet.client")

local TMP = (os.getenv("TMPDIR") or "/tmp"):gsub("/+$", "")
local FIXTURE_DIR = string.format("%s/ps-lua-ut-%d-%d", TMP, os.time(), math.random(1e6))

local FAKE_CURL = FIXTURE_DIR .. "/fake-curl"
local ARGS_FILE = FIXTURE_DIR .. "/argv"
local BODY_FILE = FIXTURE_DIR .. "/body"
local CANARY = FIXTURE_DIR .. "/canary-must-not-exist"

local function write_file(path, content)
  local f = assert(io.open(path, "wb"))
  f:write(content)
  f:close()
end

local function read_file(path)
  local f = io.open(path, "rb")
  if not f then return nil end
  local s = f:read("a")
  f:close()
  return s
end

local function exists(path)
  local f = io.open(path, "rb")
  if f then f:close() return true end
  return false
end

local function setup_fixture()
  os.execute("mkdir -p '" .. FIXTURE_DIR .. "'")
  -- The fake curl: dump every argument on its own line (base64-encoded so
  -- newlines inside an argument cannot forge extra lines), copy any
  -- --data-binary @file body aside (the transport deletes its temp file on
  -- return), then answer like curl with -w '\n%{http_code}'.
  write_file(FAKE_CURL, ([[#!/bin/sh
: > '%s'
for a in "$@"; do
  printf '%%s' "$a" | base64 >> '%s'
  printf '\n' >> '%s'
done
prev=""
for a in "$@"; do
  if [ "$prev" = "--data-binary" ]; then
    case "$a" in
      @*) cat "${a#@}" > '%s' ;;
    esac
  fi
  prev="$a"
done
printf '%%s' '{"ok":true}'
printf '\n200'
]]):format(ARGS_FILE, ARGS_FILE, ARGS_FILE, BODY_FILE))
  os.execute("chmod +x '" .. FAKE_CURL .. "'")
end

local function b64decode(line)
  local p = io.popen("printf '%s' '" .. line .. "' | base64 -d", "r")
  local out = p:read("a")
  p:close()
  return out
end

--- The argv the fake curl received, one decoded string per element
--- (element 1 is the first argument after the program name).
local function recorded_args()
  local raw = read_file(ARGS_FILE) or ""
  local args = {}
  for line in raw:gmatch("[^\n]+") do
    args[#args + 1] = b64decode(line)
  end
  return args
end

local function index_of(args, value)
  for i, a in ipairs(args) do
    if a == value then return i end
  end
  return nil
end

return function(T)
  setup_fixture()
  T.at_exit(function()
    os.execute("rm -rf '" .. FIXTURE_DIR .. "'")
  end)

  local function fresh(opts)
    os.remove(ARGS_FILE)
    os.remove(BODY_FILE)
    opts = opts or {}
    opts.curl = FAKE_CURL
    opts.server = opts.server or "http://127.0.0.1:1"
    return transport.new(opts)
  end

  -------------------------------------------------------------------------
  -- shquote (exported for tests): exact quoting behavior
  -------------------------------------------------------------------------

  T.test("shquote wraps in single quotes and escapes embedded ones", function()
    local q = transport._shquote
    T.eq(q("plain"), "'plain'")
    T.eq(q("with space"), "'with space'")
    T.eq(q("a'b"), [['a'\''b']])
    T.eq(q([['';!]]), [[''\'''\'';!']])
    T.eq(q("$(touch x) `y` ;&|"), [['$(touch x) `y` ;&|']])
    T.eq(q("한글 🍓"), "'한글 🍓'")
    T.eq(q(""), "''")
  end)

  -------------------------------------------------------------------------
  -- flag mapping
  -------------------------------------------------------------------------

  T.test("a plain GET maps to curl -X GET with the full URL last", function()
    local t = fresh()
    local status, body = t:request("GET", "/api/health")
    T.eq(status, 200)
    T.eq(body, '{"ok":true}')
    local args = recorded_args()
    T.ok(index_of(args, "-X"), "-X present")
    T.eq(args[index_of(args, "-X") + 1], "GET")
    T.eq(args[#args], "http://127.0.0.1:1/api/health", "URL is the final argument")
    T.eq(index_of(args, "--http3"), nil, "no --http3 unless asked")
    T.eq(index_of(args, "-k"), nil, "no -k unless asked")
  end)

  T.test("http3 = true maps to curl --http3", function()
    local t = fresh({ http3 = true })
    t:request("GET", "/api/health")
    T.ok(index_of(recorded_args(), "--http3"), "--http3 must be passed")
  end)

  T.test("insecure = true maps to curl -k", function()
    local t = fresh({ insecure = true })
    t:request("GET", "/api/health")
    T.ok(index_of(recorded_args(), "-k"), "-k must be passed")
  end)

  T.test("both flags combine", function()
    local t = fresh({ http3 = true, insecure = true })
    t:request("GET", "/api/health")
    local args = recorded_args()
    T.ok(index_of(args, "--http3") and index_of(args, "-k"), "both flags")
  end)

  T.test("headers become -H 'Name: value' pairs", function()
    local t = fresh()
    t:request("GET", "/api/rooms", nil, { Authorization = "Bearer tok-123" })
    local args = recorded_args()
    local i = index_of(args, "Authorization: Bearer tok-123")
    T.ok(i, "header argument present")
    T.eq(args[i - 1], "-H")
  end)

  T.test("a body adds Content-Type json and --data-binary @tempfile", function()
    local t = fresh()
    t:request("POST", "/api/rooms", '{"name":"x"}')
    local args = recorded_args()
    T.ok(index_of(args, "Content-Type: application/json"), "content type")
    local i = index_of(args, "--data-binary")
    T.ok(i, "--data-binary present")
    T.eq(args[i + 1]:sub(1, 1), "@", "body travels via a file, not argv")
    T.eq(read_file(BODY_FILE), '{"name":"x"}', "body bytes intact")
    T.ok(not exists(args[i + 1]:sub(2)), "temp body file is cleaned up")
  end)

  T.test("the -w format requests the status code on its own line", function()
    local t = fresh()
    t:request("GET", "/api/health")
    local args = recorded_args()
    local i = index_of(args, "-w")
    T.ok(i, "-w present")
    T.eq(args[i + 1], "\\n%{http_code}")
  end)

  T.test("the body temp file is created private (0600) and owner-only", function()
    -- A curl that reports the octal mode of its --data-binary file.
    local perm_curl = FIXTURE_DIR .. "/perm-curl"
    write_file(perm_curl, [[#!/bin/sh
prev=""
for a in "$@"; do
  if [ "$prev" = "--data-binary" ]; then
    f="${a#@}"
    stat -f '%Lp' "$f" 2>/dev/null || stat -c '%a' "$f" 2>/dev/null
  fi
  prev="$a"
done
printf '\n200'
]])
    os.execute("chmod +x '" .. perm_curl .. "'")
    local t = transport.new({ curl = perm_curl, server = "http://x" })
    local status, body = t:request("POST", "/x", '{"a":1}')
    T.eq(status, 200)
    T.eq((body:gsub("%s+$", "")), "600", "temp body file must be mode 0600")
  end)

  T.test("a NUL byte in the path is rejected, not silently truncated", function()
    local t = fresh()
    local status, err = t:request("GET", "/api/rooms/room\0injected")
    T.eq(status, nil)
    T.contains(err, "NUL byte")
    -- and nothing was executed
    local ran = read_file(ARGS_FILE)
    T.ok(ran == nil or ran == "", "curl must not have been invoked")
  end)

  T.test("a NUL byte in the server URL is rejected", function()
    local t = fresh({ server = "http://127.0.0.1:1/\0" })
    local status, err = t:request("GET", "/api/health")
    T.eq(status, nil)
    T.contains(err, "NUL byte")
  end)

  -------------------------------------------------------------------------
  -- hostile input stays inert (the security property)
  -------------------------------------------------------------------------

  local hostiles = {
    '"; rm -rf ~"',
    "'; touch " .. CANARY .. "; '",
    "$(touch " .. CANARY .. ")",
    "`touch " .. CANARY .. "`",
    "a && touch " .. CANARY,
    "a | tee " .. CANARY,
    "newline\nembedded",
    "unicode 한글 🍓 quote' backslash\\",
  }

  T.test("hostile message text reaches the wire verbatim and executes nothing", function()
    for _, evil in ipairs(hostiles) do
      local t = fresh()
      local body = json.encode({ content = evil, msgHash = string.rep("ab", 32) })
      local status = t:request("POST", "/api/rooms/room_x/messages", body,
        { Authorization = "Bearer tok" })
      T.eq(status, 200, "request must succeed")
      T.eq(read_file(BODY_FILE), body, "body bytes must be exact for: " .. evil)
      T.ok(not exists(CANARY), "canary must not exist after: " .. evil)
      T.eq(json.decode(read_file(BODY_FILE)).content, evil, "content round-trips")
    end
  end)

  T.test("hostile header values stay single inert arguments", function()
    for _, evil in ipairs(hostiles) do
      local t = fresh()
      t:request("GET", "/api/rooms", nil, { ["X-Test"] = evil })
      local args = recorded_args()
      T.ok(index_of(args, "X-Test: " .. evil),
        "header must arrive as one argument for: " .. evil)
      T.ok(not exists(CANARY), "canary must not exist after: " .. evil)
    end
  end)

  T.test("a hostile server URL stays one inert argument", function()
    local evil = "http://127.0.0.1:1/$(touch " .. CANARY .. ");x y"
    local t = fresh({ server = evil })
    t:request("GET", "/api/health")
    local args = recorded_args()
    T.eq(args[#args], evil .. "/api/health")
    T.ok(not exists(CANARY), "canary must not exist")
  end)

  T.test("a hostile room id in the path stays inert", function()
    local t = fresh()
    t:request("GET", "/api/rooms/`touch " .. CANARY .. "`/messages")
    T.ok(not exists(CANARY), "canary must not exist")
  end)

  -------------------------------------------------------------------------
  -- response parsing and failure modes
  -------------------------------------------------------------------------

  T.test("multi-line response bodies parse status correctly", function()
    local multi = FIXTURE_DIR .. "/multiline-curl"
    write_file(multi, "#!/bin/sh\nprintf '{\\n \"a\": 1\\n}'\nprintf '\\n201'\n")
    os.execute("chmod +x '" .. multi .. "'")
    local t = transport.new({ curl = multi, server = "http://x" })
    local status, body = t:request("GET", "/")
    T.eq(status, 201)
    T.eq(body, '{\n "a": 1\n}')
  end)

  T.test("a failing curl surfaces exit code and stderr", function()
    local bad = FIXTURE_DIR .. "/failing-curl"
    write_file(bad, "#!/bin/sh\necho 'curl: (7) Failed to connect' >&2\nexit 7\n")
    os.execute("chmod +x '" .. bad .. "'")
    local t = transport.new({ curl = bad, server = "http://x" })
    local status, err = t:request("GET", "/")
    T.eq(status, nil)
    T.contains(err, "curl failed (exit 7)")
    T.contains(err, "Failed to connect")
  end)

  T.test("unparseable curl output is an error, not a crash", function()
    local weird = FIXTURE_DIR .. "/weird-curl"
    write_file(weird, "#!/bin/sh\nprintf 'no status line here'\n")
    os.execute("chmod +x '" .. weird .. "'")
    local t = transport.new({ curl = weird, server = "http://x" })
    local status, err = t:request("GET", "/")
    T.eq(status, nil)
    T.contains(err, "could not parse")
  end)

  T.test("the Client raises when curl itself fails", function()
    local bad = FIXTURE_DIR .. "/failing-curl"
    local c = Client.new({ token = "tok", curl = bad, server = "http://x" })
    T.err_match(function() c:rooms() end, "curl failed")
  end)

  T.test("trailing slashes on the server URL are normalized", function()
    local t = fresh({ server = "http://127.0.0.1:1///" })
    t:request("GET", "/api/health")
    local args = recorded_args()
    T.eq(args[#args], "http://127.0.0.1:1/api/health")
  end)
end
