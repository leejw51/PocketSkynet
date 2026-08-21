# PocketSkynet Python client

A Python library and CLI for the PocketSkynet server: wallet login
(EIP-191 challenge signing), rooms, and plaintext messages — over either
**HTTP/1.1(+TLS)** (httpx) or **HTTP/3 over QUIC** (aioquic, ALPN `h3`).

E2EE is out of scope for this client; messages are sent as plaintext with
the required `msgHash` (SHA-256 of the trimmed content).

## Install

Python 3.11+.

```sh
cd app/clients/python
python3 -m venv .venv
.venv/bin/pip install -e '.[dev]'
```

This installs the `pocketskynet_client` package and the `pskynet` console
script. Dependencies: `httpx`, `aioquic`, `coincurve` (libsecp256k1:
RFC 6979 deterministic, low-S signatures), `pycryptodome` (Keccak-256).

## CLI

Global flags (accepted before or after the subcommand):

| Flag | Meaning |
| --- | --- |
| `--server <url>` | Server base URL (default `https://127.0.0.1:9099`, env `POCKETSKYNET_SERVER`) |
| `--http3` | Use HTTP/3 over QUIC — requires an `https://` URL |
| `--insecure` | Skip certificate verification (the dev server's self-signed cert) |
| `--key <hex>` | Wallet private key (or env `POCKETSKYNET_KEY`) |
| `--username <name>` | Username for first-time login (otherwise auto-generated) |

Commands: `login`, `rooms`, `create-room <name>`, `send <roomId> <text>`,
`messages <roomId>`, `health`.

After a `login` the JWT is cached per (server, wallet) under
`~/.config/pocketskynet-client/`, so later commands reuse it instead of
burning the 5/min login rate limit; an expired token re-runs the flow
automatically.

### HTTP/1.1 over TLS (the `make start` default deployment)

```sh
export POCKETSKYNET_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

pskynet --server https://127.0.0.1:9099 --insecure health
pskynet --server https://127.0.0.1:9099 --insecure login
pskynet --server https://127.0.0.1:9099 --insecure create-room "Team chat"
pskynet --server https://127.0.0.1:9099 --insecure rooms
pskynet --server https://127.0.0.1:9099 --insecure send room_123_abc "hello"
pskynet --server https://127.0.0.1:9099 --insecure messages room_123_abc
```

Plain HTTP (`make http`, localhost only) works too: `--server
http://127.0.0.1:9099` and drop `--insecure`.

### HTTP/3 (QUIC)

The dev server serves HTTP/3 on the same port number over UDP
(`make start`, `HTTP3=1` by default). Add `--http3`:

```sh
pskynet --server https://127.0.0.1:9099 --http3 --insecure health
pskynet --server https://127.0.0.1:9099 --http3 --insecure rooms
```

QUIC mandates TLS, so the URL must be `https://`.

## Library

Everything is `async`; the API layer is transport-agnostic — hand
`Client` either transport:

```python
import asyncio
from pocketskynet_client import Client, HttpTransport, Http3Transport

async def main():
    transport = HttpTransport("https://127.0.0.1:9099", insecure=True)
    # or: transport = Http3Transport("https://127.0.0.1:9099", insecure=True)
    async with Client(transport) as client:
        result = await client.login_with_key("0xac09...ff80")
        print(result.wallet_address, result.token[:16], "...")

        room = await client.create_room("Team chat", "optional description")
        await client.send_message(room["id"], "hello over any transport")
        for message in await client.messages(room["id"], limit=20):
            print(message["senderAddress"], message["content"])

asyncio.run(main())
```

Signing utilities live in `pocketskynet_client.crypto`
(`personal_sign`, `eip191_digest`, `private_key_to_address`, `keccak256`).

## Tests

The signing and derivation tests are pinned byte-exactly against the
canonical protocol vectors in `app/core/tests/vectors/protocol-v1.json`
(EIP-191 signatures, key → address derivation, msgHash), plus camelCase
request-serialization checks:

```sh
.venv/bin/python -m pytest
.venv/bin/ruff check src tests
```
