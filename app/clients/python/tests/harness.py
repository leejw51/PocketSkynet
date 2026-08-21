"""Boots a real `pocketskynet` process for the integration tests.

Modeled on the Rust suite's harness (app/server/tests/common/harness.rs) and
the repo's Python supervisor (app/tests/integration/supervisor.py):

- every server gets its own ephemeral port and its own temp data directory,
  torn down in a `finally`-shaped fixture finalizer whatever the outcome;
- boots are serialised behind a lock and retried, because "ask the OS for a
  free port, close it, hand the number to a child" has a window in which two
  children can be aimed at the same port -- and the loser's health probe is
  answered by the *winner*, so it would happily drive somebody else's server;
- the child is started with `--jwt-secret` so tests can mint and tamper with
  tokens themselves (expiry, wrong secret, stripped signature);
- inherited PS_* / VITE_* / POCKETSKYNET_* variables are scrubbed and
  PS_IGNORE_BAKED_ENV is set, so a developer's shell (or a `make build`
  binary's baked-in values) never decides what the suite tests.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from pocketskynet_client import Client, Http3Transport, HttpTransport
from pocketskynet_client.crypto import parse_private_key

BOOT_TIMEOUT_SECS = 30.0

# Handed to the server with --jwt-secret so tests can mint and tamper with
# tokens themselves.
TEST_JWT_SECRET = "pocketskynet-python-client-test-secret-0123456789abcdef"

# Hardhat account #0 -- also the key the protocol vectors use.
ALICE_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
# Hardhat account #1.
BOB_KEY = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"

_BOOT_LOCK = threading.Lock()


def find_server_binary() -> str:
    """A prebuilt `pocketskynet` binary, or a clear failure telling you how
    to get one. POCKETSKYNET_BIN overrides; otherwise the newest of the
    debug/release builds under app/target is used."""
    override = os.environ.get("POCKETSKYNET_BIN")
    if override:
        if Path(override).is_file():
            return override
        pytest.fail(f"POCKETSKYNET_BIN={override} does not exist")
    target = Path(__file__).resolve().parents[3] / "target"
    candidates = [
        path
        for path in (
            target / "debug" / "pocketskynet",
            target / "release" / "pocketskynet",
        )
        if path.is_file()
    ]
    if not candidates:
        pytest.fail(
            "no pocketskynet server binary found under app/target/{debug,release}/ -- "
            "build one with `cargo build -p pocketskynet-server` (from app/) or point "
            "POCKETSKYNET_BIN at one"
        )
    return str(max(candidates, key=lambda path: path.stat().st_mtime))


def free_tcp_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def free_udp_port() -> int:
    """Separate from free_tcp_port on purpose: TCP and UDP port numbers live
    in different namespaces, so probing one says nothing about the other."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class ServerProc:
    """One throwaway pocketskynet server: own port, own data dir, own log."""

    def __init__(self, binary: str, tls: bool = False, http3: bool = False):
        self.binary = binary
        self.tls = tls
        self.http3 = http3
        self.port = 0
        self.http3_port: int | None = None
        self.redirect_port: int | None = None
        self.base_url = ""
        self.root = ""
        self.data_dir = ""
        self.log_path = ""
        self.child: subprocess.Popen | None = None

    # ------------------------------------------------------------- boot --

    def start(self) -> None:
        last_error = ""
        for _ in range(5):
            with _BOOT_LOCK:
                error = self._try_start()
            if error is None:
                return
            last_error = error
            self._teardown()
        pytest.fail(f"could not start pocketskynet after 5 attempts: {last_error}")

    def _try_start(self) -> str | None:
        self.port = free_tcp_port()
        self.redirect_port = free_tcp_port() if self.tls else None
        self.http3_port = free_udp_port() if self.http3 else None
        scheme = "https" if self.tls else "http"
        self.base_url = f"{scheme}://127.0.0.1:{self.port}"
        self.root = tempfile.mkdtemp(prefix="psk-pyclient-it-")
        self.data_dir = os.path.join(self.root, "data")
        static_dir = os.path.join(self.root, "static")  # empty: nothing to leak
        self.log_path = os.path.join(self.root, "server.log")
        os.makedirs(self.data_dir)
        os.makedirs(static_dir)

        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("PS_", "VITE_", "POCKETSKYNET_"))
        }
        env["PS_IGNORE_BAKED_ENV"] = "1"

        args = [
            self.binary,
            "--host",
            "127.0.0.1",
            "--port",
            str(self.port),
            "--data-dir",
            self.data_dir,
            "--static-dir",
            static_dir,
            "--jwt-secret",
            TEST_JWT_SECRET,
            "--no-rate-limit",
            "--no-payment-verify",
            "--no-mdns",
            "--log",
            "warn",
        ]
        if self.tls:
            args += ["--tls", "--http-redirect-port", str(self.redirect_port)]
        if self.http3:
            args += ["--http3", "--http3-port", str(self.http3_port)]

        with open(self.log_path, "wb") as log:
            self.child = subprocess.Popen(args, env=env, stdout=log, stderr=log)
        return self._wait_healthy()

    @property
    def ca_path(self) -> str:
        return os.path.join(self.data_dir, "tls", "ca.crt")

    def _wait_healthy(self) -> str | None:
        """Block until /api/health answers 200, verifying that the answer came
        from *our* child (a lost bind race means somebody else answered)."""
        deadline = time.monotonic() + BOOT_TIMEOUT_SECS
        url = f"{self.base_url}/api/health"
        while time.monotonic() < deadline:
            assert self.child is not None
            if self.child.poll() is not None:
                return (
                    f"server exited during boot with code {self.child.returncode}"
                    f"\n{self._log_tail()}"
                )
            try:
                handlers: list = [urllib.request.ProxyHandler({})]
                if self.tls:
                    # Verify against the CA the server is minting; until that
                    # file is flushed the attempt fails like any other
                    # boot-in-progress error, and we retry.
                    context = ssl.create_default_context(cafile=self.ca_path)
                    handlers.append(urllib.request.HTTPSHandler(context=context))
                opener = urllib.request.build_opener(*handlers)
                with opener.open(url, timeout=2) as response:
                    if response.status == 200:
                        if self.child.poll() is not None:
                            return (
                                f"another process owns port {self.port}; our child "
                                f"exited with {self.child.returncode}"
                            )
                        return None
            except (urllib.error.URLError, OSError, ssl.SSLError, FileNotFoundError):
                pass
            time.sleep(0.05)
        return f"/api/health never became ready on port {self.port}\n{self._log_tail()}"

    # --------------------------------------------------------- teardown --

    def stop(self) -> None:
        if self.child is not None and self.child.poll() is None:
            self.child.terminate()
            try:
                self.child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait()

    def cleanup(self) -> None:
        """Stop the server and delete its directory; assert nothing strays."""
        self.stop()
        assert (
            self.child is None or self.child.poll() is not None
        ), "server still running"
        self._teardown()

    def _teardown(self) -> None:
        if self.root:
            shutil.rmtree(self.root, ignore_errors=True)
            self.root = ""

    def _log_tail(self) -> str:
        try:
            with open(self.log_path, "rb") as log:
                tail = log.read()[-3000:]
            return "--- server log (tail) ---\n" + tail.decode("utf-8", "replace")
        except OSError:
            return ""

    # ---------------------------------------------------------- clients --

    def h1_client(self, token: str | None = None, insecure: bool = True) -> Client:
        """A client over HTTP/1.1(+TLS)."""
        return Client(HttpTransport(self.base_url, insecure=insecure), token=token)

    def h3_client(self, token: str | None = None, insecure: bool = True) -> Client:
        """A client over HTTP/3 (QUIC), against the UDP listener."""
        assert self.http3_port is not None, "this server has no HTTP/3 listener"
        url = f"https://127.0.0.1:{self.http3_port}"
        return Client(Http3Transport(url, insecure=insecure), token=token)


# ------------------------------------------------------------------ JWTs --


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def mint_jwt(
    wallet_address: str,
    secret: str = TEST_JWT_SECRET,
    lifetime_secs: int = 3600,
) -> str:
    """An HS256 JWT exactly like the server's own -- possible because the
    harness handed the server its signing secret. Negative lifetimes mint
    already-expired tokens."""
    now = int(time.time())
    header = _b64url(json.dumps({"typ": "JWT", "alg": "HS256"}).encode())
    payload = _b64url(
        json.dumps(
            {
                "walletAddress": wallet_address.lower(),
                "iat": now,
                "exp": now + lifetime_secs,
            }
        ).encode()
    )
    signing_input = f"{header}.{payload}".encode("ascii")
    signature = hmac.new(secret.encode(), signing_input, hashlib.sha256).digest()
    return f"{header}.{payload}.{_b64url(signature)}"


def fresh_key() -> str:
    """A random private key nobody has logged in with yet."""
    while True:
        candidate = "0x" + os.urandom(32).hex()
        try:
            parse_private_key(candidate)
            return candidate
        except ValueError:  # pragma: no cover - ~2^-128
            continue
