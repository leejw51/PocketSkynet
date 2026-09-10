-- runner.lua — tiny test framework for the Lua client suites.
--
-- One test = one T.test(name, fn) call; assertion helpers raise with a
-- readable diff and the failure is recorded without aborting the run, so a
-- broken test can never skip another test's teardown. T.finish() prints the
-- totals, runs registered at-exit hooks (the integration harness registers
-- its kill-everything hook there), and exits nonzero on any failure.

local M = {
  passed = 0,
  failed = 0,
  skipped = 0,
  failures = {},
  _at_exit = {},
}

--- Run one named test; failures are recorded, not fatal.
function M.test(name, fn)
  local ok, err = xpcall(fn, function(e)
    return debug.traceback(tostring(e), 2)
  end)
  if ok then
    M.passed = M.passed + 1
  else
    M.failed = M.failed + 1
    M.failures[#M.failures + 1] = name
    io.write("FAIL ", name, "\n  ", tostring(err):gsub("\n", "\n  "), "\n")
  end
end

--- Record a skipped test with the reason (printed immediately).
function M.skip(name, reason)
  M.skipped = M.skipped + 1
  io.write("SKIP ", name, " — ", reason, "\n")
end

--- Assert equality with a readable message.
function M.eq(got, want, label)
  if got ~= want then
    error(string.format("%s\n  want: %s\n  got:  %s",
      label or "values differ", tostring(want), tostring(got)), 2)
  end
end

function M.ok(cond, label)
  if not cond then
    error(label or "expected truthy value", 2)
  end
end

--- Assert that fn() raises, and that the error message matches pattern.
function M.err_match(fn, pattern, label)
  local ok, err = pcall(fn)
  if ok then
    error((label or "call") .. ": expected an error, got success", 2)
  end
  err = tostring(err)
  if not err:find(pattern) then
    error(string.format("%s: error %q does not match %q",
      label or "call", err, pattern), 2)
  end
  return err
end

--- Assert a string contains a plain-text fragment.
function M.contains(haystack, needle, label)
  if not tostring(haystack):find(needle, 1, true) then
    error(string.format("%s\n  missing: %s\n  in:      %s",
      label or "substring missing", tostring(needle), tostring(haystack)), 2)
  end
end

--- Register a hook to run before the process exits (teardown safety net).
function M.at_exit(fn)
  M._at_exit[#M._at_exit + 1] = fn
end

--- Print the summary, run at-exit hooks, and exit (nonzero on failure).
function M.finish(suite)
  for _, fn in ipairs(M._at_exit) do
    pcall(fn)
  end
  local parts = { string.format("%d passed", M.passed) }
  parts[#parts + 1] = string.format("%d failed", M.failed)
  if M.skipped > 0 then
    parts[#parts + 1] = string.format("%d skipped", M.skipped)
  end
  io.write(suite or "suite", ": ", table.concat(parts, ", "), "\n")
  if M.failed > 0 then
    io.write("failed tests:\n")
    for _, name in ipairs(M.failures) do io.write("  - ", name, "\n") end
    os.exit(1)
  end
  os.exit(0)
end

return M
