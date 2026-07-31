#!/usr/bin/env bash
# Dev mode: rebuild the wasm bundle on change while the server runs against the
# same web/dist directory. Both are killed together on Ctrl-C.
set -euo pipefail

host="${1:-0.0.0.0}"
port="${2:-9099}"
data="${3:-${POCKETSKYNET_PATH:-$HOME/.pocketskynet}}"
# Non-empty turns on HTTPS, so hot reload is usable from a phone or a tablet
# too: `make dev TLS=1`.
tls_flag="${4:+--tls}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$root"

cleanup() {
    # Kill the whole process group so trunk's watcher goes down with us.
    [[ -n "${trunk_pid:-}" ]] && kill "$trunk_pid" 2>/dev/null || true
    [[ -n "${server_pid:-}" ]] && kill "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "==> trunk watch (web/dist)"
(cd web && trunk watch) &
trunk_pid=$!

# Wait for the first bundle before starting the server, otherwise the static
# handler starts up pointing at an empty directory.
for _ in $(seq 1 120); do
    [[ -f web/dist/index.html ]] && break
    sleep 0.5
done

echo "==> cargo run (server)"
cargo run -p pocketskynet-server -- \
    --host "$host" --port "$port" --data-dir "$data" --static-dir web/dist $tls_flag &
server_pid=$!

wait "$server_pid"
