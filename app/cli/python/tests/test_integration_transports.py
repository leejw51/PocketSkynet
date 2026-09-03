"""Transport parity against a real --tls --http3 server, mirroring
app/server/tests/http3.rs: the same flow over HTTP/1.1+TLS and HTTP/3, a
room written over one transport read over the other, certificate handling
(--insecure vs. verification), and connection reuse on QUIC."""

from __future__ import annotations

import asyncio

import pytest

from pocketskynet_client import Client, Http3Transport, HttpTransport
from pocketskynet_client.errors import TransportError

pytestmark = pytest.mark.integration


def run(coro):
    return asyncio.run(coro)


def _client_for(server, transport_name, token=None):
    if transport_name == "h1":
        return server.h1_client(token=token)
    return server.h3_client(token=token)


@pytest.mark.parametrize("transport_name", ["h1", "h3"])
def test_the_full_flow_over_each_transport(server, transport_name):
    """health -> login -> create room -> send -> read back, per transport."""
    from harness import fresh_key

    key = fresh_key()

    async def flow():
        async with _client_for(server, transport_name) as client:
            health = await client.health()
            assert health["status"] == "ok"
            result = await client.login_with_key(key, f"user-{transport_name}")
            assert result.token
            room = await client.create_room(f"{transport_name} room")
            sent = await client.send_message(room["id"], f"hello over {transport_name}")
            messages = await client.messages(room["id"])
            assert [m["id"] for m in messages] == [sent["id"]]
            assert messages[0]["content"] == f"hello over {transport_name}"

    run(flow())


@pytest.mark.parametrize(("write_name", "read_name"), [("h3", "h1"), ("h1", "h3")])
def test_a_room_written_over_one_transport_reads_over_the_other(
    server, alice, write_name, read_name
):
    async def flow():
        async with _client_for(server, write_name, token=alice["token"]) as writer:
            room = await writer.create_room(f"{write_name}-to-{read_name}")
            sent = await writer.send_message(room["id"], f"crossing from {write_name}")
        async with _client_for(server, read_name, token=alice["token"]) as reader:
            rooms = await reader.rooms()
            assert room["id"] in {r["id"] for r in rooms}
            messages = await reader.messages(room["id"])
            assert messages[-1]["id"] == sent["id"]
            assert messages[-1]["content"] == f"crossing from {write_name}"

    run(flow())


def test_both_listeners_answer_at_the_same_time(server, alice):
    async def flow():
        async with (
            server.h1_client(token=alice["token"]) as tcp,
            server.h3_client(token=alice["token"]) as quic,
        ):
            tcp_health, quic_health = await asyncio.gather(tcp.health(), quic.health())
            assert tcp_health["status"] == "ok"
            assert quic_health["status"] == "ok"

    run(flow())


def test_http3_requests_share_one_quic_connection(server):
    async def flow():
        async with server.h3_client() as client:
            transport = client._transport
            await client.health()
            first_protocol = transport._protocol
            assert first_protocol is not None
            await client.health()
            await client.health()
            assert transport._protocol is first_protocol

    run(flow())


def test_concurrent_http3_requests_multiplex_on_one_connection(server, alice):
    async def flow():
        async with server.h3_client(token=alice["token"]) as client:
            results = await asyncio.gather(*(client.health() for _ in range(5)))
            assert all(r["status"] == "ok" for r in results)
            assert client._transport._protocol is not None

    run(flow())


# -------------------------------------------------------- certificates --


def test_the_self_signed_cert_is_refused_without_insecure(server):
    """The dev certificate chains to a CA nobody ships; a verifying client
    must refuse it -- which is exactly what --insecure exists to bypass."""

    async def flow():
        async with Client(
            HttpTransport(server.base_url, insecure=False, timeout=5)
        ) as client:
            with pytest.raises(TransportError):
                await client.health()

    run(flow())


def test_http3_with_verification_refuses_the_unknown_ca(server):
    url = f"https://127.0.0.1:{server.http3_port}"

    async def flow():
        async with Client(Http3Transport(url, insecure=False, timeout=5)) as client:
            with pytest.raises(TransportError):
                await client.health()

    run(flow())


def test_insecure_accepts_the_self_signed_cert_on_both_transports(server):
    async def flow():
        async with server.h1_client() as h1:
            assert (await h1.health())["status"] == "ok"
        async with server.h3_client() as h3:
            assert (await h3.health())["status"] == "ok"

    run(flow())


def test_the_server_advertises_http3_via_alt_svc(server):
    """The TCP listener points clients at the QUIC one."""

    async def flow():
        transport = HttpTransport(server.base_url, insecure=True)
        try:
            response = await transport.request("GET", "/api/health")
            alt_svc = response.headers.get("alt-svc", "")
            assert 'h3=":' in alt_svc
        finally:
            await transport.aclose()

    run(flow())
