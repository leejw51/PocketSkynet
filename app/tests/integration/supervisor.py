"""Boots and tears down a throwaway pocketskynet server.

The contract `make integrationtest` promises: a backend on its own port with
its own data directory, torn down and deleted afterwards no matter how the
tests went. Hermetic on purpose — reusing a developer's ~/.pocketskynet (or
their .env, or the VITE_* values a release build bakes in) turns yesterday's
state into today's mystery failure.
"""

import os
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

BOOT_TIMEOUT_SECS = 30


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def free_udp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Backend:
    def __init__(
        self,
        binary: str,
        admin_wallet: str | None = None,
        tls: bool = False,
        http3: bool = False,
    ):
        self.binary = binary
        self.admin_wallet = admin_wallet
        self.tls = tls
        self.http3 = http3
        self.port = free_port()
        # TLS servers bring a plain-HTTP redirect listener up beside them; it
        # gets its own explicit port for the same reason the main one does.
        self.redirect_port = free_port() if tls else None
        self.http3_port = free_udp_port() if http3 else None
        scheme = "https" if tls else "http"
        self.base_url = f"{scheme}://127.0.0.1:{self.port}"
        self.root = tempfile.mkdtemp(prefix="pocketskynet-itest-")
        self.data_dir = os.path.join(self.root, "data")
        self.static_dir = os.path.join(self.root, "static")  # empty: no SPA to leak
        self.log_path = os.path.join(self.root, "server.log")
        # Written by the server on first TLS boot — the certificate is minted
        # on the fly, and this file is what a client must trust.
        self.ca_path = os.path.join(self.data_dir, "tls", "ca.crt")
        self.child = None

    def start(self):
        os.makedirs(self.data_dir)
        os.makedirs(self.static_dir)

        # The server must see only what this harness decides. PS_* and VITE_*
        # from the surrounding shell are scrubbed, and PS_IGNORE_BAKED_ENV
        # keeps a release binary's compiled-in values from granting powers the
        # tests never configured.
        env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith(("PS_", "VITE_", "POCKETSKYNET_"))
        }
        env["PS_IGNORE_BAKED_ENV"] = "1"
        if self.admin_wallet:
            env["VITE_FRUITNATION_ADMIN"] = self.admin_wallet

        args = [
            self.binary,
            "--host",
            "127.0.0.1",
            "--port",
            str(self.port),
            "--data-dir",
            self.data_dir,
            "--static-dir",
            self.static_dir,
            "--no-rate-limit",
            "--log",
            "warn",
        ]
        if self.tls:
            args += ["--tls", "--http-redirect-port", str(self.redirect_port)]
        if self.http3:
            args += ["--http3", "--http3-port", str(self.http3_port)]
        log = open(self.log_path, "wb")
        self.child = subprocess.Popen(args, env=env, stdout=log, stderr=log)
        # For a TLS boot this also proves the minted CA is on disk and valid,
        # since the probe itself verifies against it.
        self._wait_healthy()

    def _wait_healthy(self):
        deadline = time.monotonic() + BOOT_TIMEOUT_SECS
        url = f"{self.base_url}/api/health"
        while time.monotonic() < deadline:
            if self.child.poll() is not None:
                self._dump_log()
                raise RuntimeError(
                    f"server exited during boot (code {self.child.returncode})"
                )
            try:
                # Over TLS the probe verifies against the CA the server is
                # minting; until that file is flushed the attempt fails like
                # any other boot-in-progress error, and we retry. No
                # handshake in the suite ever skips verification.
                handlers = [urllib.request.ProxyHandler({})]
                if self.tls:
                    context = ssl.create_default_context(cafile=self.ca_path)
                    handlers.append(urllib.request.HTTPSHandler(context=context))
                opener = urllib.request.build_opener(*handlers)
                with opener.open(url, timeout=2) as resp:
                    if resp.status == 200:
                        return
            except (urllib.error.URLError, OSError, ssl.SSLError, FileNotFoundError):
                pass
            time.sleep(0.1)
        self._dump_log()
        raise RuntimeError(f"server did not answer {url} within {BOOT_TIMEOUT_SECS}s")

    def stop(self):
        if self.child and self.child.poll() is None:
            self.child.terminate()
            try:
                self.child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait()
        self.child = None

    def cleanup(self, keep_log_on_failure=False):
        """Stop the backend and delete its folder. Called from `finally`."""
        self.stop()
        if keep_log_on_failure:
            self._dump_log()
        shutil.rmtree(self.root, ignore_errors=True)

    def _dump_log(self):
        try:
            with open(self.log_path, "rb") as log:
                tail = log.read()[-4000:]
            if tail:
                sys.stderr.write("---- server log (tail) ----\n")
                sys.stderr.write(tail.decode("utf-8", "replace"))
                sys.stderr.write("\n---------------------------\n")
        except OSError:
            pass
