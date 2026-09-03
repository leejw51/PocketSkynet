"""Rooms against a real server, mirroring app/server/tests/rooms.rs."""

from __future__ import annotations

import asyncio

import pytest

from pocketskynet_client.errors import ApiError

pytestmark = pytest.mark.integration


def run(coro):
    return asyncio.run(coro)


def test_create_room_returns_a_bare_room(server, alice):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            room = await client.create_room("Reading group", "weekly")
            assert room["name"] == "Reading group"
            assert room["description"] == "weekly"
            assert room["id"].startswith("room_")
            assert 10 <= len(room["id"]) <= 100
            assert room["currentKeyVersion"] == 1
            assert room["keyRotationPending"] is False
            # a bare Room, not the enriched shape
            assert "members" not in room
            assert "unreadCount" not in room

    run(flow())


def test_a_created_room_appears_in_the_creators_list(server, alice):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            room = await client.create_room("Listed room")
            rooms = await client.rooms()
            match = next(r for r in rooms if r["id"] == room["id"])
            # the list is enriched: the creator is a member and an admin
            assert match["memberCount"] == 1
            member_addresses = [m["userAddress"] for m in match["members"]]
            assert member_addresses == [alice["address"]]
            admin_addresses = [a["walletAddress"] for a in match["admins"]]
            assert admin_addresses == [alice["address"]]
            assert match["unreadCount"] == 0

    run(flow())


def test_a_room_without_a_description_stores_null(server, alice):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            room = await client.create_room("No description")
            assert room["description"] is None

    run(flow())


def test_a_room_name_with_forbidden_characters_is_rejected(server, alice):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client.create_room("<script>alert(1)</script>")
            assert excinfo.value.status == 400
            assert "Validation failed" in excinfo.value.message

    run(flow())


def test_an_empty_room_name_is_rejected(server, alice):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            with pytest.raises(ApiError) as excinfo:
                await client.create_room("")
            assert excinfo.value.status == 400

    run(flow())


def test_duplicate_room_names_create_distinct_rooms(server, alice):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            first = await client.create_room("Same name")
            second = await client.create_room("Same name")
            assert first["id"] != second["id"]
            rooms = await client.rooms()
            ids = {r["id"] for r in rooms}
            assert {first["id"], second["id"]} <= ids

    run(flow())


def test_unicode_room_names_round_trip(server, alice):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            room = await client.create_room("한글 방 🍓")
            rooms = await client.rooms()
            match = next(r for r in rooms if r["id"] == room["id"])
            assert match["name"] == "한글 방 🍓"

    run(flow())


def test_room_creation_requires_authentication(server):
    async def flow():
        async with server.h1_client() as client:
            with pytest.raises(ApiError) as excinfo:
                await client.create_room("No token")
            assert excinfo.value.status == 401

    run(flow())


def test_another_wallet_does_not_see_my_rooms(server, alice, bob):
    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            room = await client.create_room("Alice private")
        async with server.h1_client(token=bob["token"]) as client:
            rooms = await client.rooms()
            assert room["id"] not in {r["id"] for r in rooms}

    run(flow())
