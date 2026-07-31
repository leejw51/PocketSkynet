#!/usr/bin/env bash
#
# Launcher shipped inside the release tarball: run PocketSkynet from wherever
# the tarball was unpacked, without needing the source tree or a Rust toolchain.
#
# It deliberately prints nothing itself. The server reports the URLs it is
# reachable on, and the security note that goes with them — a second copy here
# would be one more place for that message to drift out of date.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
port="${PORT:-9099}"
host="${HOST:-0.0.0.0}"
data="${DATA_DIR:-$here/data}"

mkdir -p "$data"

exec "$here/bin/pocketskynet" \
    --host "$host" \
    --port "$port" \
    --data-dir "$data" \
    --static-dir "$here/web"
