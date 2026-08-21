-- unit_json.lua — the bundled JSON codec: round-trips, unicode and
-- surrogate pairs, null vs omitted keys, escapes, and decode errors.

local json = require("pocketskynet.json")

return function(T)
  T.test("json round-trips a nested structure", function()
    local v = {
      name = "room",
      count = 3,
      pi = 3.5,
      flag = true,
      off = false,
      list = { 1, 2, 3 },
      nested = { deep = { x = "y" } },
    }
    local back = json.decode(json.encode(v))
    T.eq(back.name, "room")
    T.eq(back.count, 3)
    T.eq(back.pi, 3.5)
    T.eq(back.flag, true)
    T.eq(back.off, false)
    T.eq(#back.list, 3)
    T.eq(back.list[2], 2)
    T.eq(back.nested.deep.x, "y")
  end)

  T.test("json encodes integers without a decimal point", function()
    T.eq(json.encode(1749652746620), "1749652746620")
    T.eq(json.encode(0), "0")
    T.eq(json.encode(-5), "-5")
  end)

  T.test("json decodes numbers, exponents, negatives", function()
    T.eq(json.decode("42"), 42)
    T.eq(json.decode("-0.5"), -0.5)
    T.eq(json.decode("1e3"), 1000.0)
    T.eq(json.decode("2.5e-1"), 0.25)
  end)

  T.test("json string escapes round-trip", function()
    local s = "line1\nline2\ttab \"quoted\" back\\slash \r\b\f"
    T.eq(json.decode(json.encode(s)), s)
  end)

  T.test("json encodes control characters as \\u escapes", function()
    T.eq(json.encode("\1\2"), '"\\u0001\\u0002"')
    T.eq(json.encode("\0"), '"\\u0000"')
  end)

  T.test("json passes UTF-8 through verbatim", function()
    local s = "한글 메시지 🍓🍊"
    T.eq(json.encode(s), '"' .. s .. '"')
    T.eq(json.decode(json.encode(s)), s)
  end)

  T.test("json decodes \\u escapes including surrogate pairs to UTF-8", function()
    -- U+1F353 STRAWBERRY as a UTF-16 surrogate pair
    T.eq(json.decode('"\\ud83c\\udf53"'), "\240\159\141\147")
    -- BMP character (no pair)
    T.eq(json.decode('"\\ud55c"'), "\237\149\156") -- 한
    -- and the vector-file style mixed string
    T.eq(json.decode('"\\ud83c\\udf53 strawberry"'), "\240\159\141\147 strawberry")
  end)

  T.test("json rejects an unpaired surrogate", function()
    T.err_match(function() json.decode('"\\ud83c"') end, "surrogate")
    T.err_match(function() json.decode('"\\ud83cx"') end, "surrogate")
  end)

  T.test("json null decodes to the json.null sentinel, not nil", function()
    local obj = json.decode('{"a": null, "b": 1}')
    T.eq(obj.a, json.null)
    T.eq(obj.b, 1)
    -- an omitted key is nil — distinguishable from an explicit null
    T.eq(obj.c, nil)
    T.ok(obj.a ~= nil, "null must not collapse into absence")
  end)

  T.test("json.null encodes back to null; nil values are omitted", function()
    T.eq(json.encode({ a = json.null }), '{"a":null}')
    -- a nil table value simply does not exist in Lua — the key is absent
    T.eq(json.encode({ a = nil }), "{}")
  end)

  T.test("json arrays and objects are distinguished", function()
    T.eq(json.encode({ 1, 2, 3 }), "[1,2,3]")
    T.eq(json.encode({ x = 1 }), '{"x":1}')
    T.eq(json.encode({}), "{}")
    local arr = json.decode("[]")
    T.eq(#arr, 0)
    T.eq(next(json.decode("{}")), nil)
  end)

  T.test("json decodes whitespace liberally", function()
    local v = json.decode('  {\n\t"a" : [ 1 , 2 ] ,\r\n "b" : "x" }  ')
    T.eq(v.a[1], 1)
    T.eq(v.b, "x")
  end)

  T.test("json decode rejects malformed input", function()
    T.err_match(function() json.decode("") end, "unexpected")
    T.err_match(function() json.decode("{") end, "expected")
    T.err_match(function() json.decode('{"a":}') end, "unexpected")
    T.err_match(function() json.decode('{"a":1,}') end, "expected")
    T.err_match(function() json.decode("[1,2") end, "expected")
    T.err_match(function() json.decode('"abc') end, "unterminated")
    T.err_match(function() json.decode('"\\q"') end, "invalid escape")
    T.err_match(function() json.decode('"\\u12g4"') end, "u escape")
    T.err_match(function() json.decode("truex") end, "trailing garbage")
    T.err_match(function() json.decode("1 2") end, "trailing garbage")
    T.err_match(function() json.decode('{"a":1} extra') end, "trailing garbage")
  end)

  T.test("json decode rejects raw control characters inside strings", function()
    T.err_match(function() json.decode('"a\nb"') end, "control character")
  end)

  T.test("json encode rejects unencodable values", function()
    T.err_match(function() json.encode(0 / 0) end, "NaN")
    T.err_match(function() json.encode(math.huge) end, "NaN/Inf")
    T.err_match(function() json.encode(print) end, "unsupported type")
    T.err_match(function() json.encode({ [1.5] = "x" }) end, "keys must be strings")
  end)

  T.test("json round-trips the API shapes we actually send", function()
    -- exactly what the client posts on login and send
    local login = json.decode(json.encode({
      walletAddress = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
      challengeId = "6f1e2c30-aaaa-bbbb-cccc-121212121212",
      signature = "0x" .. string.rep("ab", 65),
    }))
    T.eq(login.walletAddress, "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
    T.eq(login.signature, "0x" .. string.rep("ab", 65))

    local msg = json.decode(json.encode({
      content = 'hostile "; rm -rf ~" content\'`$(x)`',
      msgHash = string.rep("ab", 32),
      isEncrypted = false,
    }))
    T.eq(msg.content, 'hostile "; rm -rf ~" content\'`$(x)`')
    T.eq(msg.isEncrypted, false)
  end)
end
