"""Wallet crypto: keccak256, key -> address, EIP-191 personal_sign.

Matches the PocketSkynet protocol (PROTOCOL.md section 4) byte-exactly:

- keccak256 is original Keccak (padding byte 0x01), NOT NIST SHA3-256.
- address = "0x" + hex(keccak256(X || Y)[12..32]) -- the leading 0x04 SEC1
  byte is dropped before hashing.
- EIP-191 digest = keccak256(0x19 || "Ethereum Signed Message:\n"
  || decimal(utf8_byte_len) || utf8(msg)).
- ECDSA over secp256k1 with RFC 6979 deterministic nonces, low-S normalized,
  serialized r(32) || s(32) || v(1) with v = recovery_id + 27; wire form is
  "0x" + 130 lowercase hex.

Signing is delegated to libsecp256k1 via coincurve, which is deterministic
(RFC 6979) and always emits normalized low-S signatures. The test suite pins
every rule above against app/core/tests/vectors/protocol-v1.json.
"""

from __future__ import annotations

from coincurve import PrivateKey
from Crypto.Hash import keccak as _keccak

__all__ = [
    "eip191_digest",
    "keccak256",
    "parse_private_key",
    "personal_sign",
    "private_key_to_address",
    "private_key_to_public_key",
]


def keccak256(data: bytes) -> bytes:
    """Original Keccak-256 (Ethereum's hash), not SHA3-256."""
    h = _keccak.new(digest_bits=256)
    h.update(data)
    return h.digest()


def parse_private_key(key: str) -> bytes:
    """Parse a hex private key, with or without the 0x prefix.

    Rejects zero, values >= the secp256k1 group order, and anything that is
    not exactly 32 bytes of hex.
    """
    s = key.strip()
    if s.startswith(("0x", "0X")):
        s = s[2:]
    if len(s) != 64:
        raise ValueError("private key must be 32 bytes (64 hex chars)")
    try:
        raw = bytes.fromhex(s)
    except ValueError as exc:
        raise ValueError("private key is not valid hex") from exc
    n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
    value = int.from_bytes(raw, "big")
    if value == 0 or value >= n:
        raise ValueError("private key out of range for secp256k1")
    return raw


def private_key_to_public_key(private_key: bytes) -> bytes:
    """Uncompressed SEC1 public key: 65 bytes, 0x04 || X || Y."""
    return PrivateKey(private_key).public_key.format(compressed=False)


def private_key_to_address(private_key: bytes) -> str:
    """Lowercase 0x-prefixed Ethereum address for a private key.

    Hashes the 64-byte X || Y (dropping the 0x04 SEC1 prefix) and takes the
    last 20 bytes.
    """
    public = private_key_to_public_key(private_key)
    return "0x" + keccak256(public[1:]).hex()[-40:]


def eip191_digest(message: str) -> bytes:
    """keccak256 over the EIP-191 personal_sign envelope.

    The length is the UTF-8 *byte* length of the message, in decimal.
    """
    payload = message.encode("utf-8")
    prefix = b"\x19Ethereum Signed Message:\n" + str(len(payload)).encode("ascii")
    return keccak256(prefix + payload)


def personal_sign(private_key: bytes, message: str) -> str:
    """EIP-191 personal_sign; returns "0x" + 130 lowercase hex chars.

    Deterministic (RFC 6979) and low-S, so the same key and message always
    produce the same signature. v is recovery_id + 27.
    """
    digest = eip191_digest(message)
    # hasher=None: sign the 32-byte digest itself, not a hash of it.
    sig = PrivateKey(private_key).sign_recoverable(digest, hasher=None)
    r_s, recovery_id = sig[:64], sig[64]
    return "0x" + r_s.hex() + bytes([recovery_id + 27]).hex()
