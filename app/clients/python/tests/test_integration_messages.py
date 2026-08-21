"""Messages against a real server, mirroring app/server/tests/messages.rs:
sending with msgHash, listing/ordering/limits, membership enforcement,
validation edges, and one concurrency check."""

from __future__ import annotations

import asyncio
import hashlib

import pytest

from pocketskynet_client.api import send_message_request
from pocketskynet_client.errors import ApiError

pytestmark = pytest.mark.integration


def run(coro):
    return asyncio.run(coro)


@pytest.fixture
def room(server, alice):
    """A fresh room per test, so message assertions never see a neighbour's."""

    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            return await client.create_room("msg-tests")

    return run(flow())


def test_a_sent_message_comes_back_with_its_sender(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            message = await client.send_message(room["id"], "Hello everyone!")
            assert message["content"] == "Hello everyone!"
            assert message["msgHash"] == hashlib.sha256(b"Hello everyone!").hexdigest()
            assert message["roomId"] == room["id"]
            assert message["msgType"] == "add"
            assert message["isDeleted"] is False
            assert message["isEncrypted"] is False
            assert message["iv"] is None and message["hmac"] is None
            # the sender comes from the JWT, not the body
            assert message["senderAddress"] == alice["address"]
            assert message["sender"]["walletAddress"] == alice["address"]

    run(flow())


def test_content_is_trimmed_before_storing(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            message = await client.send_message(room["id"], "  padded  \n")
            assert message["content"] == "padded"
            assert message["msgHash"] == hashlib.sha256(b"padded").hexdigest()

    run(flow())


def test_unicode_content_round_trips_intact(server, alice, room):
    text = "한글 메시지 🍓🍊"

    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            await client.send_message(room["id"], text)
            messages = await client.messages(room["id"])
            assert messages[-1]["content"] == text

    run(flow())


def test_messages_list_ascending_and_ordered(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            for n in range(5):
                await client.send_message(room["id"], f"message {n}")
            messages = await client.messages(room["id"])
            contents = [m["content"] for m in messages]
            assert contents == [f"message {n}" for n in range(5)]
            serials = [m["msgSerial"] for m in messages]
            assert serials == sorted(serials)
            timestamps = [m["messageTimestamp"] for m in messages]
            assert timestamps == sorted(timestamps)

    run(flow())


def test_the_limit_returns_the_newest_messages(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            for n in range(6):
                await client.send_message(room["id"], f"m{n}")
            page = await client.messages(room["id"], limit=2)
            assert [m["content"] for m in page] == ["m4", "m5"]

    run(flow())


def test_backward_pagination_with_before(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            for n in range(4):
                await client.send_message(room["id"], f"p{n}")
            newest = await client.messages(room["id"], limit=2)
            older = await client.messages(
                room["id"], limit=2, before=newest[0]["messageTimestamp"]
            )
            assert [m["content"] for m in older] == ["p0", "p1"]

    run(flow())


def test_a_non_member_cannot_send(server, bob, room):
    async def flow():
        async with server.h1_client(token=bob["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client.send_message(room["id"], "let me in")
            assert excinfo.value.status == 403
            assert excinfo.value.message == "Access denied"

    run(flow())


def test_a_non_member_cannot_read(server, bob, room):
    async def flow():
        async with server.h1_client(token=bob["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client.messages(room["id"])
            assert excinfo.value.status == 403

    run(flow())


def test_a_nonexistent_room_reads_as_403_not_404(server, alice):
    """Membership is checked before existence -- no room-existence oracle."""

    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client.messages("room_0000000000_does-not-exist")
            assert excinfo.value.status == 403

    run(flow())


def test_oversized_content_is_rejected(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client.send_message(room["id"], "x" * 5001)
            assert excinfo.value.status == 400
            assert "Validation failed" in excinfo.value.message

    run(flow())


def test_content_of_exactly_five_thousand_chars_is_accepted(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            message = await client.send_message(room["id"], "y" * 5000)
            assert len(message["content"]) == 5000

    run(flow())


def test_an_uppercase_msg_hash_is_rejected(server, alice, room):
    """msgHash is lowercase-hex-only on the wire; drive the raw body to
    prove the client's lowercase hashing is what makes sends acceptable."""

    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            body = send_message_request("case check")
            body["msgHash"] = body["msgHash"].upper()
            with pytest.raises(ApiError) as excinfo:
                await client._call("POST", f"/api/rooms/{room['id']}/messages", body)
            assert excinfo.value.status == 400

    run(flow())


def test_a_missing_msg_hash_is_rejected(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client._call(
                    "POST",
                    f"/api/rooms/{room['id']}/messages",
                    {"content": "no hash"},
                )
            assert excinfo.value.status == 400

    run(flow())


def test_whitespace_only_content_is_rejected(server, alice, room):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client.send_message(room["id"], "   \n  ")
            assert excinfo.value.status == 400

    run(flow())


def test_concurrent_sends_all_land_with_distinct_serials(server, alice, room):
    """Ten parallel sends: every one lands, every serial is unique, and the
    room's history contains all ten."""

    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            results = await asyncio.gather(
                *(client.send_message(room["id"], f"parallel {n}") for n in range(10))
            )
            assert len(results) == 10
            serials = [m["msgSerial"] for m in results]
            assert len(set(serials)) == 10
            ids = {m["id"] for m in results}
            assert len(ids) == 10
            history = await client.messages(room["id"], limit=100)
            assert ids <= {m["id"] for m in history}
            # The list is ordered by (messageTimestamp, msgSerial) -- not by
            # serial alone: concurrent sends sharing a millisecond can land
            # with serial order and timestamp order interleaved.
            pairs = [(m["messageTimestamp"], m["msgSerial"]) for m in history]
            assert pairs == sorted(pairs)

    run(flow())
