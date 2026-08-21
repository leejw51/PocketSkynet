"""Request serialization: camelCase field names and msgHash rules."""

from __future__ import annotations

import hashlib
import json

from pocketskynet_client.api import (
    challenge_request,
    create_room_request,
    login_request,
    message_hash,
    send_message_request,
)


def test_challenge_request_is_camel_case():
    body = challenge_request("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
    assert body == {"walletAddress": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"}


def test_login_request_is_camel_case():
    body = login_request(
        wallet_address="0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        challenge_id="6f1e2c30-0000-0000-0000-000000000000",
        signature="0xabcdef",
        username="alice",
    )
    assert set(body) == {"walletAddress", "challengeId", "signature", "username"}
    # never the snake_case Python argument names
    assert "wallet_address" not in json.dumps(body)
    assert "challenge_id" not in json.dumps(body)


def test_login_request_omits_empty_username():
    body = login_request("0x" + "ab" * 20, "cid", "0x00")
    assert "username" not in body


def test_create_room_request_omits_absent_description():
    assert create_room_request("Team chat") == {"name": "Team chat"}
    assert create_room_request("Team chat", "hi") == {
        "name": "Team chat",
        "description": "hi",
    }


def test_send_message_request_shape():
    body = send_message_request("Hello everyone!")
    assert set(body) == {"content", "msgHash"}
    assert body["content"] == "Hello everyone!"
    assert body["msgHash"] == hashlib.sha256(b"Hello everyone!").hexdigest()


def test_message_hash_is_sha256_of_trimmed_content(vectors):
    """The server trims content before storing; msgHash must hash the trimmed
    string (PROTOCOL.md section 13) with plain SHA-256, never keccak."""
    assert send_message_request("  padded  ")["content"] == "padded"
    assert message_hash("  padded  ") == hashlib.sha256(b"padded").hexdigest()
    # cross-check against the canonical plaintext msgHash vectors (they
    # include a whitespace-padded case and a Unicode case)
    plaintext_vectors = vectors["msgHash"]["plaintext"]
    assert len(plaintext_vectors) >= 3
    for vector in plaintext_vectors:
        assert message_hash(vector["content"]) == vector["msgHashHex"], vector[
            "content"
        ]


def test_msg_hash_wire_form():
    digest = message_hash("hi")
    assert len(digest) == 64
    assert digest == digest.lower()
    int(digest, 16)  # valid hex
