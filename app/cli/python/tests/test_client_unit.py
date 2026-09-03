"""The API client against a fake transport: what goes on the wire, and how
responses -- happy, null-laden, malformed, and every error envelope shape --
are parsed. No server, no sockets."""

from __future__ import annotations

import asyncio
import hashlib
import json

import pytest

from pocketskynet_client.api import Client
from pocketskynet_client.errors import ApiError
from pocketskynet_client.transport import Transport, TransportResponse

ALICE = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
# Hardhat #0 -- the key behind ALICE, for login-flow tests.
ALICE_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"


def challenge_message(address: str, nonce: str = "0" * 64) -> str:
    """A challenge shaped like the server's (API.md section 6.2.1) -- the
    client's signing guard only signs something starting with this template
    and naming the wallet."""
    return (
        "Welcome to FruitNation!\n\n"
        "Click to sign in and accept the FruitNation Terms of Service.\n\n"
        "This request will not trigger a blockchain transaction or cost any "
        "gas fees.\n\n"
        f"Wallet address:\n{address}\n\nNonce:\n{nonce}"
    )


class FakeTransport(Transport):
    """Records every request; answers from a scripted queue (or a handler)."""

    def __init__(self, responses=None, handler=None):
        self.requests: list[dict] = []
        self.responses = list(responses or [])
        self.handler = handler
        self.closed = False

    async def request(self, method, path, headers=None, body=None):
        record = {
            "method": method,
            "path": path,
            "headers": {k.lower(): v for k, v in (headers or {}).items()},
            "body": body,
        }
        self.requests.append(record)
        if self.handler is not None:
            return self.handler(record)
        return self.responses.pop(0)

    async def aclose(self):
        self.closed = True


def ok(payload, status=200) -> TransportResponse:
    return TransportResponse(status, {}, json.dumps(payload).encode())


def run(coro):
    return asyncio.run(coro)


# ------------------------------------------------------------- the wire --


def test_authenticated_request_carries_bearer_token():
    transport = FakeTransport([ok([])])
    run(Client(transport, token="tok123").rooms())
    request = transport.requests[0]
    assert request["method"] == "GET"
    assert request["path"] == "/api/rooms"
    assert request["headers"]["authorization"] == "Bearer tok123"
    assert request["body"] is None


def test_unauthenticated_endpoints_never_send_a_token():
    transport = FakeTransport([ok({"status": "ok", "uptime": 1})])
    run(Client(transport, token="tok123").health())
    assert "authorization" not in transport.requests[0]["headers"]


def test_no_token_means_no_authorization_header():
    transport = FakeTransport([ok([])])
    run(Client(transport).rooms())
    assert "authorization" not in transport.requests[0]["headers"]


def test_post_bodies_are_camel_case_json_with_content_type():
    transport = FakeTransport([ok({"id": "room_x"})])
    run(Client(transport, token="t").create_room("Team chat", "desc"))
    request = transport.requests[0]
    assert request["headers"]["content-type"] == "application/json"
    body = json.loads(request["body"])
    assert body == {"name": "Team chat", "description": "desc"}


def test_send_message_trims_and_hashes_on_the_wire():
    transport = FakeTransport([ok({"id": "msg_x"})])
    run(Client(transport, token="t").send_message("room_abcdefgh12", "  hi there  "))
    request = transport.requests[0]
    assert request["path"] == "/api/rooms/room_abcdefgh12/messages"
    body = json.loads(request["body"])
    assert body["content"] == "hi there"
    assert body["msgHash"] == hashlib.sha256(b"hi there").hexdigest()


def test_messages_query_parameters():
    transport = FakeTransport([ok([]), ok([])])
    client = Client(transport, token="t")
    run(client.messages("room_abcdefgh12", limit=7))
    assert (
        transport.requests[0]["path"] == "/api/rooms/room_abcdefgh12/messages?limit=7"
    )
    run(client.messages("room_abcdefgh12", limit=5, before=123, since=45))
    assert (
        transport.requests[1]["path"]
        == "/api/rooms/room_abcdefgh12/messages?limit=5&before=123&since=45"
    )


def test_unicode_survives_json_encoding():
    transport = FakeTransport([ok({})])
    run(Client(transport, token="t").send_message("room_abcdefgh12", "한글 🍓"))
    body = json.loads(transport.requests[0]["body"].decode("utf-8"))
    assert body["content"] == "한글 🍓"


def test_context_manager_closes_the_transport():
    transport = FakeTransport([ok({"status": "ok"})])

    async def flow():
        async with Client(transport) as client:
            await client.health()

    run(flow())
    assert transport.closed


# ------------------------------------------------------- room-id guard --


@pytest.mark.parametrize(
    "bad_room_id",
    [
        "../../admin",  # path traversal -> would POST to /admin/messages
        "a#x",  # fragment -> would truncate the path before /messages
        "room?a=1",  # query injection
        "room id",  # space
        "short",  # under 10 chars
        "x" * 101,  # over 100 chars
        "room/../x",  # embedded slash
    ],
)
def test_send_rejects_a_malformed_room_id_without_touching_the_wire(bad_room_id):
    transport = FakeTransport([ok({})])
    with pytest.raises(ValueError, match="invalid room id"):
        run(Client(transport, token="t").send_message(bad_room_id, "hi"))
    assert transport.requests == []  # nothing left the client


@pytest.mark.parametrize("bad_room_id", ["../../admin", "a#x", "short"])
def test_messages_rejects_a_malformed_room_id_without_touching_the_wire(bad_room_id):
    transport = FakeTransport([ok([])])
    with pytest.raises(ValueError, match="invalid room id"):
        run(Client(transport, token="t").messages(bad_room_id))
    assert transport.requests == []


def test_a_well_formed_room_id_is_accepted():
    from pocketskynet_client.api import validate_room_id

    # the real server shape, dots included (roomId allows [A-Za-z0-9_.-])
    for good in [
        "room_1749652739650_304e0eaf-bcf9-4682-a6a0-69bee8e40b97",
        "room_0000000000_does-not-exist",
        "a.b_c-d123",
    ]:
        assert validate_room_id(good) == good


# -------------------------------------------------------- login plumbing --


def _login_handler(record):
    """A minimal fake server for the challenge -> login flow."""
    if record["path"] == "/api/auth/challenge":
        body = json.loads(record["body"])
        assert body == {"walletAddress": ALICE}
        return ok(
            {
                "challengeId": "cid-1",
                "message": challenge_message(ALICE),
                "expiresAt": "2099-01-01T00:00:00.000Z",
            }
        )
    if record["path"] == "/api/auth/login":
        body = json.loads(record["body"])
        assert body["walletAddress"] == ALICE
        assert body["challengeId"] == "cid-1"
        assert body["signature"].startswith("0x") and len(body["signature"]) == 132
        return ok(
            {"user": {"walletAddress": ALICE, "username": "alice"}, "token": "jwt-1"}
        )
    raise AssertionError(f"unexpected {record['path']}")


def test_login_with_key_signs_the_challenge_and_stores_the_token():
    transport = FakeTransport(handler=_login_handler)
    client = Client(transport)
    result = run(client.login_with_key(ALICE_KEY, "alice"))
    assert result.token == "jwt-1"
    assert result.wallet_address == ALICE
    assert client.token == "jwt-1"
    assert [r["path"] for r in transport.requests] == [
        "/api/auth/challenge",
        "/api/auth/login",
    ]


def test_first_login_retries_with_a_generated_username():
    """A username-required 400 burns the challenge; the retry fetches a fresh
    one and carries a deterministic default username."""
    state = {"challenges": 0}

    def handler(record):
        if record["path"] == "/api/auth/challenge":
            state["challenges"] += 1
            return ok(
                {
                    "challengeId": f"cid-{state['challenges']}",
                    "message": challenge_message(
                        ALICE, nonce=str(state["challenges"]) * 64
                    ),
                    "expiresAt": "2099-01-01T00:00:00.000Z",
                }
            )
        body = json.loads(record["body"])
        if "username" not in body:
            return TransportResponse(
                400, {}, b'{"message":"Username is required for first-time login"}'
            )
        assert body["username"] == "user_" + ALICE[2:10]
        assert body["challengeId"] == "cid-2"  # a fresh challenge, not the burned one
        return ok(
            {
                "user": {"walletAddress": ALICE, "username": body["username"]},
                "token": "t2",
            }
        )

    client = Client(FakeTransport(handler=handler))
    result = run(client.login_with_key(ALICE_KEY))
    assert result.token == "t2"
    assert state["challenges"] == 2


def test_signing_is_refused_for_a_non_login_challenge():
    """A malicious/MITM server must not get the wallet to sign an arbitrary
    string (e.g. an E2EE-key-derivation message) via the login flow."""

    def handler(record):
        assert record["path"] == "/api/auth/challenge"  # never reaches login
        return ok(
            {
                "challengeId": "cid",
                "message": (
                    "FruitNation Encryption Key Derivation v2\n\nAddress: "
                    f"{ALICE}\nSalt: 00\nPurpose: End-to-end encryption only"
                ),
                "expiresAt": "2099-01-01T00:00:00.000Z",
            }
        )

    transport = FakeTransport(handler=handler)
    with pytest.raises(ValueError, match="refusing to sign"):
        run(Client(transport).login_with_key(ALICE_KEY, "alice"))
    assert [r["path"] for r in transport.requests] == ["/api/auth/challenge"]


def test_signing_is_refused_when_the_challenge_names_another_wallet():
    other = "0x" + "ab" * 20

    def handler(record):
        return ok(
            {
                "challengeId": "cid",
                "message": challenge_message(other),  # not ALICE
                "expiresAt": "2099-01-01T00:00:00.000Z",
            }
        )

    with pytest.raises(ValueError, match="refusing to sign"):
        run(Client(FakeTransport(handler=handler)).login_with_key(ALICE_KEY, "alice"))


def test_other_login_failures_are_not_retried():
    def handler(record):
        if record["path"] == "/api/auth/challenge":
            return ok(
                {
                    "challengeId": "cid",
                    "message": challenge_message(ALICE),
                    "expiresAt": "2099-01-01T00:00:00.000Z",
                }
            )
        return TransportResponse(401, {}, b'{"message":"Invalid signature"}')

    transport = FakeTransport(handler=handler)
    with pytest.raises(ApiError) as excinfo:
        run(Client(transport).login_with_key(ALICE_KEY, "alice"))
    assert excinfo.value.status == 401
    assert len(transport.requests) == 2  # one challenge, one login, no retry


# ------------------------------------------------------ response parsing --


def test_nulls_in_responses_are_preserved():
    message = {
        "id": "msg_1",
        "content": "hi",
        "iv": None,
        "hmac": None,
        "editedAt": None,
        "sender": {"publicKey": None, "publicKeySig": None},
    }
    result = run(
        Client(FakeTransport([ok([message])]), token="t").messages("room_abcdefgh12")
    )
    assert result[0]["iv"] is None
    assert result[0]["editedAt"] is None
    assert result[0]["sender"]["publicKeySig"] is None


def test_error_envelope_plain_message():
    transport = FakeTransport(
        [TransportResponse(403, {}, b'{"message":"Access denied"}')]
    )
    with pytest.raises(ApiError) as excinfo:
        run(Client(transport, token="t").rooms())
    error = excinfo.value
    assert (error.status, error.message, error.code) == (403, "Access denied", None)


def test_error_envelope_with_validation_errors_array():
    body = json.dumps(
        {
            "message": "Validation failed",
            "errors": ["roomId: Room ID contains invalid characters", "name: Required"],
        }
    ).encode()
    transport = FakeTransport([TransportResponse(400, {}, body)])
    with pytest.raises(ApiError) as excinfo:
        run(Client(transport, token="t").create_room("x"))
    assert "Validation failed" in excinfo.value.message
    assert "roomId: Room ID contains invalid characters" in excinfo.value.message
    assert "name: Required" in excinfo.value.message


def test_error_envelope_with_machine_readable_code():
    body = json.dumps(
        {
            "code": "STALE_KEY_VERSION",
            "message": "Message key version does not match",
            "currentKeyVersion": 4,
        }
    ).encode()
    transport = FakeTransport([TransportResponse(409, {}, body)])
    with pytest.raises(ApiError) as excinfo:
        run(Client(transport, token="t").send_message("room_abcdefgh12", "hi"))
    assert excinfo.value.code == "STALE_KEY_VERSION"
    assert excinfo.value.status == 409
    assert "STALE_KEY_VERSION" in str(excinfo.value)


def test_non_json_error_body_falls_back_to_request_failed():
    transport = FakeTransport(
        [TransportResponse(500, {}, b"<html>Internal Server Error</html>")]
    )
    with pytest.raises(ApiError) as excinfo:
        run(Client(transport, token="t").rooms())
    assert excinfo.value.message == "Request failed"


def test_empty_error_body_falls_back_to_request_failed():
    transport = FakeTransport([TransportResponse(502, {}, b"")])
    with pytest.raises(ApiError) as excinfo:
        run(Client(transport, token="t").rooms())
    assert (excinfo.value.status, excinfo.value.message) == (502, "Request failed")


def test_malformed_json_on_success_yields_none_not_a_crash():
    transport = FakeTransport([TransportResponse(200, {}, b"not json {")])
    assert run(Client(transport, token="t").rooms()) is None


def test_status_edges_of_the_2xx_window():
    assert run(
        Client(FakeTransport([ok({"a": 1}, status=299)]), token="t").rooms()
    ) == {"a": 1}
    with pytest.raises(ApiError):
        run(
            Client(
                FakeTransport([TransportResponse(300, {}, b"{}")]), token="t"
            ).rooms()
        )
