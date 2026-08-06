#!/usr/bin/env bash
# Run the end-to-end suite against a throwaway PocketSkynet server.
#
# Hermetic on purpose. An earlier version reused whatever database was already
# there, and a run that suspended an account and then failed before reinstating
# it left every later run signing in as a suspended user — failures that looked
# like product bugs and were nothing but yesterday's state. A fresh data
# directory per run is the only version of this that can be trusted.
set -euo pipefail

# Resolved before anything changes directory. `dirname "$0"` after a `cd` gives
# "." for a relatively-invoked script, which silently ran Playwright from the
# wrong directory — it then globbed the whole `app/` tree, loaded
# `tests/browser/*.js` as if they were specs, and failed with an error that
# pointed at this suite.
SUITE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Default to the repository this script lives in, not a hard-coded path.
APP_DIR="${APP_DIR:-$(cd "$SUITE_DIR/../.." && pwd)}"
# Not 9399: that is the port used for poking at the app by hand, and an
# already-bound port is the one failure this script cannot survive quietly —
# the server it starts would die, the health check would be answered by
# *someone else's* server, and the suite would silently test the wrong
# database. Hence the explicit check below rather than trusting the default.
PORT="${PORT:-9401}"
DATA_DIR="${DATA_DIR:-$(mktemp -d)/psdata}"
# The wallet the suite administers with. Must match `boss` in helpers.js —
# that pairing is the entire point of the admin tests, because it proves the
# role really does come from this environment variable.
ADMIN_WALLET="${ADMIN_WALLET:-0xac550F3DA533F335f33ED7a316b2D361DF03919F}"

cleanup() {
  [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "port $PORT is already in use — refusing to run against a server this" >&2
  echo "script did not start. Stop it, or set PORT=…" >&2
  exit 1
fi

mkdir -p "$DATA_DIR"
cd "$APP_DIR"

# `.env` for the chain metadata the paid routes need, then the admin list is
# overridden so the suite does not depend on who the developer happens to be.
set -a
[[ -f .env ]] && . ./.env
set +a
export VITE_FRUITNATION_ADMIN="$ADMIN_WALLET"

# Rate limiting off: the suite drives a dozen wallets from one address, which
# is precisely the case `--no-rate-limit` documents itself as existing for.
./target/debug/pocketskynet \
  --host 127.0.0.1 --port "$PORT" \
  --data-dir "$DATA_DIR" --static-dir web/dist \
  --no-rate-limit >"$DATA_DIR/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 40); do
  if curl -fsS "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1; then break; fi
  sleep 0.25
done
curl -fsS "http://127.0.0.1:$PORT/api/health" >/dev/null || {
  echo "server did not come up:"; cat "$DATA_DIR/server.log"; exit 1;
}
grep -i "server administrators" "$DATA_DIR/server.log" || true

cd "$SUITE_DIR"
PS_BASE="http://127.0.0.1:$PORT" npx playwright test "$@"
