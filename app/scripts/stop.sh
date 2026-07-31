#!/usr/bin/env bash
#
# Stop a running PocketSkynet.
#
# Identified by *what is listening on the port*, not by name alone: that is what
# `make start` actually produced, and it avoids killing an unrelated process that
# merely shares a word in its command line. The name is still checked before
# signalling, so a port collision with something else is reported rather than
# acted on.
#
# SIGTERM, not SIGKILL. The server drains connections and flushes the JSONL
# event log on the way out; killing it outright would lose the tail of the log
# for no reason. SIGKILL is the fallback if it will not go.
set -euo pipefail

PORT="${1:-9099}"
GRACE_SECONDS=10

listeners() {
    lsof -ti "tcp:${PORT}" -sTCP:LISTEN 2>/dev/null || true
}

name_of() {
    ps -p "$1" -o comm= 2>/dev/null || true
}

pids=$(listeners)

if [[ -z "$pids" ]]; then
    echo "  nothing is listening on port ${PORT}"
    exit 0
fi

stopped=0
for pid in $pids; do
    name=$(name_of "$pid")
    case "$name" in
        *pocketskynet*)
            echo "  stopping ${name##*/} (pid $pid) on port ${PORT}"
            kill -TERM "$pid" 2>/dev/null || true

            # Give it time to drain and flush before escalating.
            for _ in $(seq 1 $((GRACE_SECONDS * 2))); do
                kill -0 "$pid" 2>/dev/null || break
                sleep 0.5
            done

            if kill -0 "$pid" 2>/dev/null; then
                echo "  did not exit within ${GRACE_SECONDS}s; forcing"
                kill -KILL "$pid" 2>/dev/null || true
            fi
            stopped=$((stopped + 1))
            ;;
        "")
            echo "  pid $pid vanished before it could be stopped"
            ;;
        *)
            echo "  port ${PORT} is held by '${name}' (pid $pid), which is not PocketSkynet"
            echo "  refusing to kill it — stop it yourself, or use: make stop PORT=<other>"
            exit 1
            ;;
    esac
done

[[ $stopped -gt 0 ]] && echo "  stopped"
exit 0
