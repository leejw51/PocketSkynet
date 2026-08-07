#!/usr/bin/env python3
"""`make integrationtest` — supervise throwaway backends and run every flow.

Two phases, each hermetic: a plain-HTTP backend for the full flow suite,
then a `--tls --http3` backend whose self-signed certificate is minted on
the fly, for the TLS and QUIC checks. Each backend gets its own port and
data directory, and both are stopped and deleted afterwards — pass or fail.
No third-party packages: everything down to the wallet signatures is stdlib.

Usage: python3 tests/integration/run.py [flow-name-substring ...]
       POCKETSKYNET_BIN=path/to/pocketskynet overrides the binary.
"""

import os
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from ethwallet import Wallet
from flows import FLOWS, TLS_FLOWS, Context, TlsContext
from supervisor import Backend

APP_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DEFAULT_BIN = os.path.join(APP_DIR, "target", "debug", "pocketskynet")


def run_flows(ctx, flows) -> int:
    failures = 0
    for flow in flows:
        started = time.monotonic()
        try:
            flow(ctx)
            print(f"  ok   {flow.__name__}  ({time.monotonic() - started:.1f}s)")
        except Exception:
            failures += 1
            print(f"  FAIL {flow.__name__}")
            traceback.print_exc()
    return failures


def main() -> int:
    binary = os.environ.get("POCKETSKYNET_BIN", DEFAULT_BIN)
    if not os.path.isfile(binary):
        print(f"server binary not found: {binary}", file=sys.stderr)
        print("build it first: cargo build -p pocketskynet-server", file=sys.stderr)
        return 2

    wanted = sys.argv[1:]

    def matching(flows):
        return [f for f in flows if not wanted or any(w in f.__name__ for w in wanted)]

    flows, tls_flows = matching(FLOWS), matching(TLS_FLOWS)
    if not flows and not tls_flows:
        print(f"no flow matches {wanted}", file=sys.stderr)
        return 2

    failures = 0

    if flows:
        # The admin wallet exists before boot because being a server admin is
        # configuration: the address goes into VITE_FRUITNATION_ADMIN and the
        # flows prove that a deployment configured this way really produces one.
        admin_wallet = Wallet()
        backend = Backend(binary, admin_wallet=admin_wallet.address)
        print(f"==> backend on {backend.base_url}, data in {backend.root}")
        try:
            backend.start()
            ctx = Context(backend.base_url, admin_wallet)
            ctx.setup()
            failures += run_flows(ctx, flows)
        finally:
            backend.cleanup(keep_log_on_failure=failures > 0)

    if tls_flows:
        before = failures
        tls_backend = Backend(binary, tls=True, http3=True)
        print(
            f"==> TLS+HTTP/3 backend on {tls_backend.base_url}, data in {tls_backend.root}"
        )
        try:
            tls_backend.start()
            failures += run_flows(TlsContext(tls_backend), tls_flows)
        finally:
            tls_backend.cleanup(keep_log_on_failure=failures > before)

    total = len(flows) + len(tls_flows)
    print(f"==> {total - failures}/{total} flows passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
