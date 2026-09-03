"""Shared fixtures.

Unit tests get `vectors` -- the canonical protocol test vectors, generated
and validated by the Rust suite (app/core/tests/vectors/protocol-v1.json); a
port is correct only when it reproduces them byte-for-byte.

Integration tests (marked `integration`) additionally get one real
`pocketskynet` server for the whole session, started with --tls --http3 so
both transports can be exercised against the self-signed-certificate
deployment, plus two logged-in wallets. The server harness lives in
harness.py; its fixture finalizer stops the process and asserts it is gone.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest
from harness import ALICE_KEY, BOB_KEY, ServerProc, find_server_binary

VECTORS_PATH = (
    Path(__file__).resolve().parents[3]
    / "core"
    / "tests"
    / "vectors"
    / "protocol-v1.json"
)


@pytest.fixture(scope="session")
def vectors() -> dict:
    return json.loads(VECTORS_PATH.read_text())


# ------------------------------------------------------------ integration --


@pytest.fixture(scope="session")
def server():
    """One TLS + HTTP/3 server for the whole integration run."""
    proc = ServerProc(find_server_binary(), tls=True, http3=True)
    proc.start()
    yield proc
    proc.cleanup()
    assert proc.child is not None and proc.child.poll() is not None, "server leaked"


def _login(server: ServerProc, key: str, username: str) -> dict:
    async def flow() -> dict:
        async with server.h1_client() as client:
            result = await client.login_with_key(key, username)
            return {
                "key": key,
                "address": result.wallet_address,
                "token": result.token,
                "username": result.user.get("username"),
            }

    return asyncio.run(flow())


@pytest.fixture(scope="session")
def alice(server) -> dict:
    """A logged-in wallet: {key, address, token, username}. The token is a
    plain string, so per-test event loops can share it freely."""
    return _login(server, ALICE_KEY, "alice")


@pytest.fixture(scope="session")
def bob(server) -> dict:
    return _login(server, BOB_KEY, "bob")
