"""Auth against a real server: the challenge -> sign -> login flow and the
token-rejection edges, mirroring app/server/tests/auth.rs. The server was
started with the harness's --jwt-secret, so tests can mint and tamper with
tokens themselves."""

from __future__ import annotations

import asyncio

import pytest
from harness import TEST_JWT_SECRET, fresh_key, mint_jwt

from pocketskynet_client.crypto import (
    parse_private_key,
    personal_sign,
    private_key_to_address,
)
from pocketskynet_client.errors import ApiError

pytestmark = pytest.mark.integration


def run(coro):
    return asyncio.run(coro)


def test_health_answers_without_authentication(server):
    async def flow():
        async with server.h1_client() as client:
            body = await client.health()
            assert body["status"] == "ok"
            assert isinstance(body["uptime"], int)

    run(flow())


def test_login_happy_path_returns_a_working_token(server, alice):
    assert alice["address"] == private_key_to_address(parse_private_key(alice["key"]))
    assert alice["username"] == "alice"
    assert alice["token"].count(".") == 2  # a JWT

    async def flow():
        async with server.h1_client(token=alice["token"]) as client:
            rooms = await client.rooms()
            assert isinstance(rooms, list)

    run(flow())


def test_challenge_message_is_addressed_to_the_wallet(server, alice):
    async def flow():
        async with server.h1_client() as client:
            challenge = await client.challenge(alice["address"])
            assert set(challenge) >= {"challengeId", "message", "expiresAt"}
            assert challenge["message"].startswith("Welcome to FruitNation!\n\n")
            assert alice["address"] in challenge["message"]

    run(flow())


def test_first_login_without_a_username_autogenerates_one(server):
    key = fresh_key()
    address = private_key_to_address(parse_private_key(key))

    async def flow():
        async with server.h1_client() as client:
            result = await client.login_with_key(key)  # no username given
            assert result.wallet_address == address
            assert result.user["username"] == "user_" + address[2:10]

    run(flow())


def test_a_later_login_reuses_the_stored_username(server):
    key = fresh_key()

    async def flow():
        async with server.h1_client() as client:
            first = await client.login_with_key(key, "firstname")
            assert first.user["username"] == "firstname"
        async with server.h1_client() as client:
            again = await client.login_with_key(key)  # username omitted
            assert again.user["username"] == "firstname"

    run(flow())


def test_a_signature_from_the_wrong_wallet_is_a_401(server, alice, bob):
    async def flow():
        async with server.h1_client() as client:
            challenge = await client.challenge(alice["address"])
            wrong = personal_sign(parse_private_key(bob["key"]), challenge["message"])
            with pytest.raises(ApiError) as excinfo:
                await client.login(alice["address"], challenge["challengeId"], wrong)
            assert excinfo.value.status == 401
            assert excinfo.value.message == "Invalid signature"

    run(flow())


def test_a_challenge_cannot_be_reused(server, alice):
    async def flow():
        async with server.h1_client() as client:
            challenge = await client.challenge(alice["address"])
            signature = personal_sign(
                parse_private_key(alice["key"]), challenge["message"]
            )
            await client.login(alice["address"], challenge["challengeId"], signature)
            # same challengeId, same (valid!) signature -- burned
            with pytest.raises(ApiError) as excinfo:
                await client.login(
                    alice["address"], challenge["challengeId"], signature
                )
            assert excinfo.value.status == 400
            assert "challenge" in excinfo.value.message.lower()

    run(flow())


def test_a_failed_login_also_burns_the_challenge(server, alice, bob):
    async def flow():
        async with server.h1_client() as client:
            challenge = await client.challenge(alice["address"])
            wrong = personal_sign(parse_private_key(bob["key"]), challenge["message"])
            with pytest.raises(ApiError):
                await client.login(alice["address"], challenge["challengeId"], wrong)
            # even the right signature cannot ride the burned challenge
            right = personal_sign(parse_private_key(alice["key"]), challenge["message"])
            with pytest.raises(ApiError) as excinfo:
                await client.login(alice["address"], challenge["challengeId"], right)
            assert excinfo.value.status == 400

    run(flow())


# ----------------------------------------------------------- JWT edges --


def _rooms_with_token(server, token):
    async def flow():
        async with server.h1_client(token=token) as client:
            return await client.rooms()

    return run(flow())


def test_a_minted_token_with_the_known_secret_is_accepted(server, alice):
    """Proves the server signs with the --jwt-secret the harness handed it."""
    rooms = _rooms_with_token(server, mint_jwt(alice["address"]))
    assert isinstance(rooms, list)


def test_a_missing_token_is_rejected(server):
    with pytest.raises(ApiError) as excinfo:
        _rooms_with_token(server, None)
    assert excinfo.value.status == 401
    assert excinfo.value.message == "No token provided"


def test_a_tampered_signature_is_rejected(server, alice):
    token = alice["token"]
    head, _, signature = token.rpartition(".")
    flipped = ("A" if signature[0] != "A" else "B") + signature[1:]
    with pytest.raises(ApiError) as excinfo:
        _rooms_with_token(server, f"{head}.{flipped}")
    assert excinfo.value.status == 401
    assert excinfo.value.message == "Invalid token"


def test_a_token_signed_with_the_wrong_secret_is_rejected(server, alice):
    assert TEST_JWT_SECRET != "some-other-secret"
    bad = mint_jwt(alice["address"], secret="some-other-secret")
    with pytest.raises(ApiError) as excinfo:
        _rooms_with_token(server, bad)
    assert excinfo.value.status == 401


def test_an_expired_token_is_rejected(server, alice):
    expired = mint_jwt(alice["address"], lifetime_secs=-3600)
    with pytest.raises(ApiError) as excinfo:
        _rooms_with_token(server, expired)
    assert excinfo.value.status == 401
    assert excinfo.value.message == "Invalid token"


def test_a_token_with_a_stripped_signature_is_rejected(server, alice):
    head, _, _ = alice["token"].rpartition(".")
    with pytest.raises(ApiError) as excinfo:
        _rooms_with_token(server, f"{head}.")
    assert excinfo.value.status == 401
