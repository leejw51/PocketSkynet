"""EIP-191 personal_sign against the canonical eip191[] vectors.

The signatures are deterministic (RFC 6979, low-S, v = recid + 27), so
every byte must match -- a wrong nonce scheme, high-S output, or a
character-count length prefix all fail here.
"""

from __future__ import annotations

from coincurve import PrivateKey

from pocketskynet_client.crypto import (
    eip191_digest,
    keccak256,
    parse_private_key,
    personal_sign,
    private_key_to_address,
)


def _wallet_key_for(vectors, address: str) -> str:
    """The wallet private key behind a vector address, via privateKeyImports."""
    for entry in vectors["wallet"]["privateKeyImports"]:
        if entry["address"] == address:
            return entry["privateKeyHex"]
    raise AssertionError(f"no known key for {address}")


def test_vector_file_has_multiple_eip191_entries(vectors):
    assert len(vectors["eip191"]) >= 5


def test_eip191_digests(vectors):
    for vector in vectors["eip191"]:
        assert eip191_digest(vector["message"]).hex() == vector["digestHex"], vector[
            "name"
        ]


def test_eip191_utf8_byte_length_not_char_count(vectors):
    unicode_vector = next(
        v for v in vectors["eip191"] if v["name"] == "unicode-length-is-bytes"
    )
    message = unicode_vector["message"]
    assert len(message.encode("utf-8")) == unicode_vector["messageUtf8Len"]
    assert len(message) != unicode_vector["messageUtf8Len"]
    assert eip191_digest(message).hex() == unicode_vector["digestHex"]


def test_eip191_signatures_byte_exact(vectors):
    for vector in vectors["eip191"]:
        key = parse_private_key(vector["privateKeyHex"])
        signature = personal_sign(key, vector["message"])
        assert signature == vector["signatureHex"], vector["name"]


def test_signature_wire_form(vectors):
    vector = vectors["eip191"][0]
    signature = personal_sign(
        parse_private_key(vector["privateKeyHex"]), vector["message"]
    )
    assert signature.startswith("0x")
    assert len(signature) == 132  # 0x + 130 hex chars
    assert signature == signature.lower()
    v = int(signature[-2:], 16)
    assert v in (27, 28)
    # low-S: s <= n/2
    n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
    s = int(signature[66:130], 16)
    assert s <= n // 2


def test_signer_addresses_match_vectors(vectors):
    for vector in vectors["eip191"]:
        key = parse_private_key(vector["privateKeyHex"])
        assert private_key_to_address(key) == vector["address"], vector["name"]


def test_login_challenge_vector_signs_verbatim(vectors):
    """The login-challenge vector is the exact flow the client runs."""
    vector = next(v for v in vectors["eip191"] if v["name"] == "login-challenge")
    assert vector["message"].startswith("Welcome to FruitNation!\n\n")
    key = parse_private_key(vector["privateKeyHex"])
    assert personal_sign(key, vector["message"]) == vector["signatureHex"]


def test_key_binding_signatures_byte_exact(vectors):
    """keyBindings[] messages are wallet-signed EIP-191; same rules apply."""
    assert len(vectors["keyBindings"]) >= 2
    for vector in vectors["keyBindings"]:
        key = parse_private_key(_wallet_key_for(vectors, vector["address"]))
        assert personal_sign(key, vector["message"]) == vector["signatureHex"]
        assert vector["message"].startswith("FruitNation Public Key Binding\n\n")


def test_encryption_key_derivation_v2_from_signature(vectors):
    """encPriv = keccak256 of the 65 RAW signature bytes (not the 0x string),
    encPub = the uncompressed point -- pinned end to end by the v2 vectors."""
    for vector in vectors["encryptionKeyDerivation"]["v2"]:
        wallet_key = parse_private_key(_wallet_key_for(vectors, vector["address"]))
        signature = personal_sign(wallet_key, vector["message"])
        enc_priv = keccak256(bytes.fromhex(signature[2:]))
        assert "0x" + enc_priv.hex() == vector["encryptionPrivateKeyHex"]
        enc_pub = PrivateKey(enc_priv).public_key.format(compressed=False)
        assert enc_pub.hex() == vector["encryptionPublicKeyHex"]


def test_digest_of_empty_message():
    # length prefix is the decimal byte length -- "0" here, nothing after it
    assert eip191_digest("") == keccak256(b"\x19Ethereum Signed Message:\n0")


def test_digest_length_prefix_is_decimal_bytes_not_padded():
    # a 100-byte message gets the three ASCII digits "100"
    message = "a" * 100
    assert eip191_digest(message) == keccak256(
        b"\x19Ethereum Signed Message:\n100" + message.encode()
    )


def test_signing_is_deterministic_across_calls(vectors):
    vector = vectors["eip191"][0]
    key = parse_private_key(vector["privateKeyHex"])
    assert personal_sign(key, vector["message"]) == personal_sign(
        key, vector["message"]
    )
