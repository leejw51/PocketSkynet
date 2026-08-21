"""Key -> address derivation against the wallet vectors.

Pins the classic porting traps: keccak256 (not SHA3-256), hashing X || Y
without the 0x04 SEC1 prefix, taking the last 20 bytes, lowercase output.
"""

from __future__ import annotations

import pytest

from pocketskynet_client.crypto import (
    keccak256,
    parse_private_key,
    private_key_to_address,
    private_key_to_public_key,
)


def test_keccak256_is_original_keccak_not_sha3():
    # keccak256("") -- the well-known Ethereum empty hash.
    assert (
        keccak256(b"").hex()
        == "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    )


def test_private_key_imports_derive_vector_addresses(vectors):
    imports = vectors["wallet"]["privateKeyImports"]
    assert len(imports) >= 2
    for vector in imports:
        key = parse_private_key(vector["privateKeyHex"])
        assert private_key_to_address(key) == vector["address"], vector["privateKeyHex"]


def test_private_key_imports_derive_vector_public_keys(vectors):
    for vector in vectors["wallet"]["privateKeyImports"]:
        key = parse_private_key(vector["privateKeyHex"])
        public = private_key_to_public_key(key)
        assert public.hex() == vector["publicKeyUncompressedHex"]
        assert public[0] == 0x04 and len(public) == 65


def test_derived_accounts_key_to_address(vectors):
    """The HD-derived accounts' (privateKeyHex, address) pairs also pin the
    key -> address step, independent of BIP-32 derivation."""
    for vector in vectors["wallet"]["accounts"]:
        key = parse_private_key(vector["privateKeyHex"])
        assert private_key_to_address(key) == vector["address"], vector["path"]


def test_addresses_are_lowercase(vectors):
    for vector in vectors["wallet"]["privateKeyImports"]:
        address = private_key_to_address(parse_private_key(vector["privateKeyHex"]))
        assert address == address.lower()
        assert address.startswith("0x") and len(address) == 42


def test_parse_private_key_rejects_bad_input():
    with pytest.raises(ValueError):
        parse_private_key("0x" + "00" * 32)  # zero
    with pytest.raises(ValueError):
        parse_private_key("0x" + "ff" * 32)  # >= group order
    with pytest.raises(ValueError):
        parse_private_key("0x1234")  # wrong length
    with pytest.raises(ValueError):
        parse_private_key("zz" * 32)  # not hex


def test_parse_private_key_accepts_with_and_without_prefix(vectors):
    hex_key = vectors["wallet"]["privateKeyImports"][0]["privateKeyHex"]
    assert parse_private_key(hex_key) == parse_private_key(hex_key[2:])
