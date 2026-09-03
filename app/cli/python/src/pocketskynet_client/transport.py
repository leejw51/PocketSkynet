"""Transports: the same request/response interface over HTTP/1.1 and HTTP/3.

The API layer (`api.Client`) talks only to `Transport`; which wire the bytes
travel on is decided once, at construction:

- `HttpTransport` -- HTTP/1.1(+TLS) via httpx.
- `Http3Transport` -- HTTP/3 over QUIC via aioquic (H3Connection, ALPN "h3").

Both accept `insecure=True` to skip certificate verification, for the
server's self-signed development certificates (`make start` generates one
into the data dir on first run).
"""

from __future__ import annotations

import asyncio
import contextlib
import ssl
from dataclasses import dataclass, field
from typing import Self
from urllib.parse import urlsplit

import httpx
from aioquic.asyncio.client import connect as quic_connect
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import DataReceived, H3Event, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import ConnectionTerminated, QuicEvent, StreamDataReceived

from .errors import TransportError

__all__ = ["Http3Transport", "HttpTransport", "Transport", "TransportResponse"]


@dataclass
class TransportResponse:
    status: int
    headers: dict[str, str]
    body: bytes


class Transport:
    """Abstract transport: one HTTP request in, one response out."""

    async def request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes | None = None,
    ) -> TransportResponse:
        raise NotImplementedError

    async def aclose(self) -> None:  # pragma: no cover - trivial
        pass

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()


class HttpTransport(Transport):
    """HTTP/1.1 (optionally over TLS) via httpx."""

    def __init__(self, base_url: str, insecure: bool = False, timeout: float = 30.0):
        self._client = httpx.AsyncClient(
            base_url=base_url.rstrip("/"),
            verify=not insecure,
            timeout=timeout,
        )

    async def request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes | None = None,
    ) -> TransportResponse:
        try:
            response = await self._client.request(
                method, path, headers=headers or {}, content=body
            )
        except httpx.HTTPError as exc:
            raise TransportError(str(exc)) from exc
        return TransportResponse(
            status=response.status_code,
            headers=dict(response.headers),
            body=response.content,
        )

    async def aclose(self) -> None:
        await self._client.aclose()


@dataclass
class _PendingResponse:
    status: int | None = None
    headers: dict[str, str] = field(default_factory=dict)
    body: bytearray = field(default_factory=bytearray)
    done: asyncio.Event = field(default_factory=asyncio.Event)


class _H3ClientProtocol(QuicConnectionProtocol):
    """Minimal HTTP/3 client protocol on top of aioquic's H3Connection."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._http = H3Connection(self._quic)
        self._pending: dict[int, _PendingResponse] = {}

    async def perform_request(
        self,
        method: str,
        authority: str,
        path: str,
        headers: dict[str, str],
        body: bytes | None,
    ) -> TransportResponse:
        stream_id = self._quic.get_next_available_stream_id()
        try:
            path_bytes = path.encode("ascii")
        except UnicodeEncodeError as exc:
            # A non-ASCII path should never reach here (callers validate ids),
            # but turn it into a clean client error rather than a codec
            # traceback if one ever does.
            raise TransportError(f"request path is not ASCII: {path!r}") from exc
        h3_headers = [
            (b":method", method.encode("ascii")),
            (b":scheme", b"https"),
            (b":authority", authority.encode("idna")),
            (b":path", path_bytes),
        ]
        for name, value in headers.items():
            h3_headers.append((name.lower().encode("ascii"), value.encode("latin-1")))
        pending = _PendingResponse()
        self._pending[stream_id] = pending
        self._http.send_headers(stream_id, h3_headers, end_stream=body is None)
        if body is not None:
            self._http.send_data(stream_id, body, end_stream=True)
        self.transmit()
        try:
            await pending.done.wait()
        finally:
            self._pending.pop(stream_id, None)
        if pending.status is None:
            raise TransportError("HTTP/3 stream ended without a response")
        return TransportResponse(
            status=pending.status,
            headers=pending.headers,
            body=bytes(pending.body),
        )

    def quic_event_received(self, event: QuicEvent) -> None:
        for h3_event in self._http.handle_event(event):
            self._h3_event_received(h3_event)
        # H3Connection only flags stream_ended on HeadersReceived/DataReceived
        # for *known* frame types. A server that ends the stream with a
        # trailing GREASE (reserved) frame -- as PocketSkynet's does -- has the
        # FIN swallowed along with the ignored frame, so also watch the
        # QUIC-level end-of-stream: by the time handle_event returns, every H3
        # event for the stream has been delivered.
        if isinstance(event, StreamDataReceived) and event.end_stream:
            pending = self._pending.get(event.stream_id)
            if pending is not None:
                pending.done.set()
        elif isinstance(event, ConnectionTerminated):
            for pending in self._pending.values():
                pending.done.set()

    def _h3_event_received(self, event: H3Event) -> None:
        if isinstance(event, HeadersReceived):
            pending = self._pending.get(event.stream_id)
            if pending is None:
                return
            for name, value in event.headers:
                if name == b":status":
                    pending.status = int(value.decode("ascii"))
                elif not name.startswith(b":"):
                    pending.headers[name.decode("ascii")] = value.decode("latin-1")
            if event.stream_ended:
                pending.done.set()
        elif isinstance(event, DataReceived):
            pending = self._pending.get(event.stream_id)
            if pending is None:
                return
            pending.body += event.data
            if event.stream_ended:
                pending.done.set()

    def connection_lost(self, exc: Exception | None) -> None:
        for pending in self._pending.values():
            pending.done.set()
        super().connection_lost(exc)


class Http3Transport(Transport):
    """HTTP/3 over QUIC via aioquic. The URL scheme must be https."""

    def __init__(self, base_url: str, insecure: bool = False, timeout: float = 30.0):
        parts = urlsplit(base_url)
        if parts.scheme != "https":
            raise ValueError(
                "HTTP/3 requires an https:// server URL (QUIC has no plaintext mode)"
            )
        if not parts.hostname:
            raise ValueError(f"no host in server URL {base_url!r}")
        self._host = parts.hostname
        self._port = parts.port or 443
        self._insecure = insecure
        self._timeout = timeout
        self._connect_cm = None
        self._protocol: _H3ClientProtocol | None = None
        self._lock = asyncio.Lock()

    async def _ensure_connected(self) -> _H3ClientProtocol:
        async with self._lock:
            if self._protocol is not None and not self._protocol._closed.is_set():
                return self._protocol
            # The previous connection died; close its context manager before
            # replacing it so a reconnect does not leak the old endpoint.
            if self._connect_cm is not None:
                with contextlib.suppress(Exception):  # teardown of a dead conn
                    await self._connect_cm.__aexit__(None, None, None)
                self._connect_cm = None
                self._protocol = None
            configuration = QuicConfiguration(
                is_client=True, alpn_protocols=list(H3_ALPN)
            )
            if self._insecure:
                configuration.verify_mode = ssl.CERT_NONE
            self._connect_cm = quic_connect(
                self._host,
                self._port,
                configuration=configuration,
                create_protocol=_H3ClientProtocol,
            )
            try:
                self._protocol = await asyncio.wait_for(
                    self._connect_cm.__aenter__(), self._timeout
                )
            except (TimeoutError, OSError) as exc:
                self._connect_cm = None
                raise TransportError(
                    f"QUIC connect to {self._host}:{self._port}: {exc}"
                ) from exc
            return self._protocol

    async def request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes | None = None,
    ) -> TransportResponse:
        protocol = await self._ensure_connected()
        authority = self._host if self._port == 443 else f"{self._host}:{self._port}"
        try:
            return await asyncio.wait_for(
                protocol.perform_request(method, authority, path, headers or {}, body),
                self._timeout,
            )
        except TimeoutError as exc:
            raise TransportError(
                f"HTTP/3 request timed out after {self._timeout}s"
            ) from exc

    async def aclose(self) -> None:
        if self._connect_cm is not None:
            await self._connect_cm.__aexit__(None, None, None)
            self._connect_cm = None
            self._protocol = None
