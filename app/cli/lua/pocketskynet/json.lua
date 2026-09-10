-- json.lua — minimal, dependency-free JSON encoder/decoder for Lua 5.4+.
--
-- Bundled so the client has zero luarocks dependencies. Handles everything
-- the PocketSkynet API and the protocol vector file need: nested objects and
-- arrays, full string escapes including \uXXXX with UTF-16 surrogate pairs
-- (decoded to UTF-8 bytes — the vectors contain emoji), integers and floats,
-- true/false/null. JSON null decodes to json.null (a sentinel) so it is
-- distinguishable from an absent key; encoding json.null emits null.

local M = {}

M.null = setmetatable({}, { __tostring = function() return "null" end })

---------------------------------------------------------------------------
-- Encoding
---------------------------------------------------------------------------

local escape_map = {
  ['"'] = '\\"', ["\\"] = "\\\\", ["\b"] = "\\b", ["\f"] = "\\f",
  ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t",
}

local function escape_char(c)
  return escape_map[c] or string.format("\\u%04x", c:byte())
end

local function encode_string(s)
  return '"' .. s:gsub('[%z\1-\31\\"]', escape_char) .. '"'
end

local function is_array(t)
  local n = 0
  for k in pairs(t) do
    if type(k) ~= "number" or k ~= math.floor(k) or k < 1 then return false end
    if k > n then n = k end
  end
  -- dense check
  for i = 1, n do
    if t[i] == nil then return false end
  end
  return true, n
end

local encode_value

local function encode_table(t)
  if t == M.null then return "null" end
  local arr, n = is_array(t)
  local parts = {}
  if arr and next(t) ~= nil then
    for i = 1, n do parts[i] = encode_value(t[i]) end
    return "[" .. table.concat(parts, ",") .. "]"
  end
  if next(t) == nil then
    -- Ambiguous empty table: emit an object; the API never needs [].
    return "{}"
  end
  for k, v in pairs(t) do
    if type(k) ~= "string" then
      error("json.encode: object keys must be strings")
    end
    parts[#parts + 1] = encode_string(k) .. ":" .. encode_value(v)
  end
  return "{" .. table.concat(parts, ",") .. "}"
end

encode_value = function(v)
  local tv = type(v)
  if tv == "nil" then
    return "null"
  elseif tv == "boolean" then
    return v and "true" or "false"
  elseif tv == "number" then
    if v ~= v or v == math.huge or v == -math.huge then
      error("json.encode: cannot encode NaN/Inf")
    end
    if math.type(v) == "integer" then return string.format("%d", v) end
    return string.format("%.17g", v)
  elseif tv == "string" then
    return encode_string(v)
  elseif tv == "table" then
    return encode_table(v)
  end
  error("json.encode: unsupported type " .. tv)
end

function M.encode(v)
  return encode_value(v)
end

---------------------------------------------------------------------------
-- Decoding
---------------------------------------------------------------------------

local function decode_error(str, pos, msg)
  local line, col = 1, 1
  for i = 1, math.min(pos - 1, #str) do
    if str:byte(i) == 10 then line = line + 1; col = 1 else col = col + 1 end
  end
  error(string.format("json.decode: %s at line %d col %d", msg, line, col), 0)
end

local function skip_ws(str, pos)
  local _, e = str:find("^[ \t\r\n]*", pos)
  return e + 1
end

local unescape_map = {
  ['"'] = '"', ["\\"] = "\\", ["/"] = "/", b = "\b", f = "\f",
  n = "\n", r = "\r", t = "\t",
}

local function decode_string(str, pos)
  -- pos points at the opening quote
  local out, i = {}, pos + 1
  while true do
    local c = str:byte(i)
    if not c then decode_error(str, i, "unterminated string") end
    if c == 34 then -- closing quote
      return table.concat(out), i + 1
    elseif c == 92 then -- backslash
      local esc = str:sub(i + 1, i + 1)
      if esc == "u" then
        local hex = str:sub(i + 2, i + 5)
        if not hex:match("^%x%x%x%x$") then
          decode_error(str, i, "invalid \\u escape")
        end
        local cp = tonumber(hex, 16)
        i = i + 6
        if cp >= 0xD800 and cp <= 0xDBFF then
          -- high surrogate: a low surrogate must follow
          local lo_hex = str:match("^\\u(%x%x%x%x)", i)
          local lo = lo_hex and tonumber(lo_hex, 16)
          if lo and lo >= 0xDC00 and lo <= 0xDFFF then
            cp = 0x10000 + (cp - 0xD800) * 0x400 + (lo - 0xDC00)
            i = i + 6
          else
            decode_error(str, i, "unpaired UTF-16 surrogate")
          end
        end
        out[#out + 1] = utf8.char(cp)
      else
        local mapped = unescape_map[esc]
        if not mapped then decode_error(str, i, "invalid escape \\" .. esc) end
        out[#out + 1] = mapped
        i = i + 2
      end
    elseif c < 32 then
      decode_error(str, i, "control character in string")
    else
      -- consume a run of plain characters at once
      local j = i
      repeat j = j + 1; c = str:byte(j) until not c or c == 34 or c == 92 or c < 32
      out[#out + 1] = str:sub(i, j - 1)
      i = j
    end
  end
end

local decode_value

local function decode_array(str, pos)
  local arr, n = {}, 0
  pos = skip_ws(str, pos + 1)
  if str:sub(pos, pos) == "]" then return arr, pos + 1 end
  while true do
    local v
    v, pos = decode_value(str, pos)
    n = n + 1
    arr[n] = v
    pos = skip_ws(str, pos)
    local c = str:sub(pos, pos)
    if c == "]" then return arr, pos + 1 end
    if c ~= "," then decode_error(str, pos, "expected ',' or ']'") end
    pos = skip_ws(str, pos + 1)
  end
end

local function decode_object(str, pos)
  local obj = {}
  pos = skip_ws(str, pos + 1)
  if str:sub(pos, pos) == "}" then return obj, pos + 1 end
  while true do
    if str:sub(pos, pos) ~= '"' then decode_error(str, pos, "expected string key") end
    local key, val
    key, pos = decode_string(str, pos)
    pos = skip_ws(str, pos)
    if str:sub(pos, pos) ~= ":" then decode_error(str, pos, "expected ':'") end
    pos = skip_ws(str, pos + 1)
    val, pos = decode_value(str, pos)
    obj[key] = val
    pos = skip_ws(str, pos)
    local c = str:sub(pos, pos)
    if c == "}" then return obj, pos + 1 end
    if c ~= "," then decode_error(str, pos, "expected ',' or '}'") end
    pos = skip_ws(str, pos + 1)
  end
end

decode_value = function(str, pos)
  local c = str:sub(pos, pos)
  if c == '"' then return decode_string(str, pos) end
  if c == "{" then return decode_object(str, pos) end
  if c == "[" then return decode_array(str, pos) end
  if c == "t" and str:sub(pos, pos + 3) == "true" then return true, pos + 4 end
  if c == "f" and str:sub(pos, pos + 4) == "false" then return false, pos + 5 end
  if c == "n" and str:sub(pos, pos + 3) == "null" then return M.null, pos + 4 end
  local num = str:match("^-?%d+%.?%d*[eE]?[+-]?%d*", pos)
  if num and #num > 0 then
    local v = tonumber(num)
    if v then return v, pos + #num end
  end
  decode_error(str, pos, "unexpected character '" .. c .. "'")
end

function M.decode(str)
  local pos = skip_ws(str, 1)
  local v
  v, pos = decode_value(str, pos)
  pos = skip_ws(str, pos)
  if pos <= #str then decode_error(str, pos, "trailing garbage") end
  return v
end

return M
