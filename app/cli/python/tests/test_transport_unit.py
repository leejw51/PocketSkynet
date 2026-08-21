"""Transport-layer units: URL validation, error mapping, and the HTTP/3
protocol's event handling -- including the GREASE-swallowed FIN that only
shows at the QUIC layer. No server, no sockets (except one connection-refused
probe against a loopback port that nothing listens on)."""

from __future__ import annotations

import asyncio

import pytest
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.events import ConnectionTerminated, StreamDataReceived

from pocketskynet_client.errors import TransportError
from pocketskynet_client.transport import (
    Http3Transport,
    HttpTransport,
    _H3ClientProtocol,
    _PendingResponse,
)


def run(coro):
    return asyncio.run(coro)


# --------------------------------------------------------- construction --


def test_http3_transport_requires_https():
    with pytest.raises(ValueError, match="https"):
        Http3Transport("http://127.0.0.1:9099")


def test_http3_transport_requires_a_host():
    with pytest.raises(ValueError):
        Http3Transport("https://")


def test_http3_transport_defaults_to_port_443():
    assert Http3Transport("https://example.test")._port == 443
    assert Http3Transport("https://example.test:9099")._port == 9099


def test_connection_refused_maps_to_transport_error():
    import socket

    # a port that was just bound and closed: nothing listens there
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    transport = HttpTransport(f"http://127.0.0.1:{port}", timeout=2.0)

    async def flow():
        try:
            with pytest.raises(TransportError):
                await transport.request("GET", "/api/health")
        finally:
            await transport.aclose()

    run(flow())


# --------------------------------------------- HTTP/3 protocol plumbing --


class _StubH3:
    """Stands in for aioquic's H3Connection: hands back pre-scripted H3
    events per QUIC event, exactly as handle_event would."""

    def __init__(self, script):
        self.script = list(script)

    def handle_event(self, event):
        return self.script.pop(0) if self.script else []


def _protocol_with(script, stream_ids=(0,)):
    protocol = _H3ClientProtocol.__new__(_H3ClientProtocol)
    protocol._http = _StubH3(script)
    protocol._pending = {stream_id: _PendingResponse() for stream_id in stream_ids}
    return protocol


HEADERS = [
    (b":status", b"200"),
    (b"content-type", b"application/json"),
    (b"x-has-more", b"false"),
]


def test_headers_and_data_accumulate_and_normal_fin_completes():
    protocol = _protocol_with(
        [
            [
                HeadersReceived(headers=HEADERS, stream_id=0, stream_ended=False),
                DataReceived(data=b'{"status":', stream_id=0, stream_ended=False),
            ],
            [DataReceived(data=b'"ok"}', stream_id=0, stream_ended=True)],
        ]
    )
    pending = protocol._pending[0]
    protocol.quic_event_received(
        StreamDataReceived(data=b"x", end_stream=False, stream_id=0)
    )
    assert pending.status == 200
    assert pending.headers == {
        "content-type": "application/json",
        "x-has-more": "false",
    }
    assert not pending.done.is_set()
    protocol.quic_event_received(
        StreamDataReceived(data=b"y", end_stream=True, stream_id=0)
    )
    assert bytes(pending.body) == b'{"status":"ok"}'
    assert pending.done.is_set()


def test_grease_swallowed_fin_still_completes_the_response():
    """PocketSkynet's server ends the stream with a trailing GREASE frame;
    aioquic drops the FIN with the ignored frame, so no H3 event ever says
    stream_ended -- the QUIC-level end_stream must finish the request."""
    protocol = _protocol_with(
        [
            # all H3 events arrive with stream_ended=False...
            [
                HeadersReceived(headers=HEADERS, stream_id=0, stream_ended=False),
                DataReceived(data=b'{"status":"ok"}', stream_id=0, stream_ended=False),
            ],
            # ...and the packet carrying the FIN yields no H3 events at all
        ],
    )
    pending = protocol._pending[0]
    protocol.quic_event_received(
        StreamDataReceived(data=b"payload", end_stream=True, stream_id=0)
    )
    assert pending.status == 200
    assert bytes(pending.body) == b'{"status":"ok"}'
    assert pending.done.is_set()


def test_end_stream_on_an_unknown_stream_is_ignored():
    protocol = _protocol_with([[]], stream_ids=(0,))
    protocol.quic_event_received(
        StreamDataReceived(data=b"", end_stream=True, stream_id=8)
    )
    assert not protocol._pending[0].done.is_set()


def test_connection_terminated_releases_every_pending_request():
    protocol = _protocol_with([[]], stream_ids=(0, 4))
    protocol.quic_event_received(
        ConnectionTerminated(error_code=0, frame_type=None, reason_phrase="")
    )
    assert all(pending.done.is_set() for pending in protocol._pending.values())


def test_a_dead_connection_with_no_response_raises_transport_error():
    """perform_request must not hand back a half-response: the connection
    dying before any headers arrive is a TransportError, not a status."""

    class _StubQuic:
        def get_next_available_stream_id(self):
            return 0

    class _SendOnlyH3(_StubH3):
        def send_headers(self, stream_id, headers, end_stream=False):
            pass

        def send_data(self, stream_id, data, end_stream=False):
            pass

    async def flow():
        protocol = _protocol_with([])
        protocol._quic = _StubQuic()
        protocol._http = _SendOnlyH3([])
        # transmit() is where the request would hit the wire; simulate the
        # connection dying right there, before any response bytes.
        protocol.transmit = lambda: protocol.quic_event_received(
            ConnectionTerminated(error_code=1, frame_type=None, reason_phrase="gone")
        )
        with pytest.raises(TransportError, match="without a response"):
            await protocol.perform_request(
                "GET", "127.0.0.1:1", "/api/health", {}, None
            )
        assert protocol._pending == {}  # the stream was reaped

    run(flow())


def test_a_non_ascii_path_is_a_clean_error_not_a_codec_traceback():
    """Callers validate ids, but a stray non-ASCII :path must surface as a
    TransportError, never a UnicodeEncodeError from deep in the encoder."""

    class _StubQuic:
        def get_next_available_stream_id(self):
            return 0

    async def flow():
        protocol = _protocol_with([])
        protocol._quic = _StubQuic()
        protocol._pending = {}  # the guard must fire before anything registers
        with pytest.raises(TransportError, match="not ASCII"):
            await protocol.perform_request(
                "GET", "127.0.0.1:1", "/api/rooms/한글/messages", {}, None
            )
        assert protocol._pending == {}  # nothing was registered for the stream

    run(flow())


def test_pseudo_headers_are_not_surfaced():
    protocol = _protocol_with(
        [
            [
                HeadersReceived(
                    headers=[(b":status", b"404"), (b"content-length", b"2")],
                    stream_id=0,
                    stream_ended=True,
                )
            ]
        ]
    )
    protocol.quic_event_received(
        StreamDataReceived(data=b"", end_stream=False, stream_id=0)
    )
    pending = protocol._pending[0]
    assert pending.status == 404
    assert ":status" not in pending.headers
    assert pending.headers == {"content-length": "2"}
    assert pending.done.is_set()
