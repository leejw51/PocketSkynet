"""A minimal Ethereum wallet in pure stdlib Python.

The integration suite signs in the way a real wallet does — EIP-191
personal_sign over the server's challenge — and pulling in eth_account for
that would make `make test` depend on a pip install. Everything the server
verifies (core/src/eip191.rs) is deterministic and small enough to implement
here: original Keccak-256 (0x01 padding, not NIST SHA3), secp256k1 ECDSA with
RFC 6979 nonces, low-S normalization, and the 65-byte r||s||v wire format
with v = 27 + recovery_id.

Determinism matters beyond reproducibility: the E2EE identity is literally
keccak256(signature_bytes), so a randomized nonce would derive a different
encryption key on every login.
"""

import hashlib
import hmac
import secrets

# --- Keccak-256 (the original submission, as Ethereum uses) ----------------

_KECCAK_RC = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000,
    0x000000000000808B, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
    0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800A, 0x800000008000000A,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
]

# Rotation offsets for the ρ step, indexed [x][y] with lane A[x][y] = state[x + 5*y].
_KECCAK_ROT = [
    [0, 36, 3, 41, 18],
    [1, 44, 10, 45, 2],
    [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
]

_MASK64 = (1 << 64) - 1


def _rotl64(value, shift):
    return ((value << shift) | (value >> (64 - shift))) & _MASK64


def _keccak_f1600(state):
    for rc in _KECCAK_RC:
        # θ
        c = [state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20]
             for x in range(5)]
        d = [c[(x - 1) % 5] ^ _rotl64(c[(x + 1) % 5], 1) for x in range(5)]
        for x in range(5):
            for y in range(5):
                state[x + 5 * y] ^= d[x]
        # ρ and π
        b = [0] * 25
        for x in range(5):
            for y in range(5):
                b[y + 5 * ((2 * x + 3 * y) % 5)] = _rotl64(state[x + 5 * y], _KECCAK_ROT[x][y])
        # χ
        for x in range(5):
            for y in range(5):
                state[x + 5 * y] = b[x + 5 * y] ^ ((~b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y])
        # ι
        state[0] ^= rc


def keccak256(data: bytes) -> bytes:
    rate = 136  # 1088-bit rate for a 256-bit digest
    state = [0] * 25
    # Absorb. Keccak pads with 0x01…0x80 (SHA-3 would use 0x06).
    padded = bytearray(data)
    pad_len = rate - (len(padded) % rate)
    padded += b"\x01" + b"\x00" * (pad_len - 2) + b"\x80" if pad_len >= 2 else b"\x81"
    for block_start in range(0, len(padded), rate):
        block = padded[block_start:block_start + rate]
        for i in range(rate // 8):
            state[i] ^= int.from_bytes(block[8 * i:8 * i + 8], "little")
        _keccak_f1600(state)
    # Squeeze: 32 bytes fit inside one rate, no second permutation needed.
    return b"".join(state[i].to_bytes(8, "little") for i in range(4))


# --- secp256k1 --------------------------------------------------------------

_P = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F
_N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
_GX = 0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798
_GY = 0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8


def _inv(a, m):
    return pow(a, m - 2, m)


def _point_add(p1, p2):
    if p1 is None:
        return p2
    if p2 is None:
        return p1
    x1, y1 = p1
    x2, y2 = p2
    if x1 == x2:
        if (y1 + y2) % _P == 0:
            return None
        slope = (3 * x1 * x1) * _inv(2 * y1, _P) % _P
    else:
        slope = (y2 - y1) * _inv(x2 - x1, _P) % _P
    x3 = (slope * slope - x1 - x2) % _P
    y3 = (slope * (x1 - x3) - y1) % _P
    return (x3, y3)


def _point_mul(k, point):
    result = None
    addend = point
    while k:
        if k & 1:
            result = _point_add(result, addend)
        addend = _point_add(addend, addend)
        k >>= 1
    return result


def _rfc6979_k(digest: bytes, priv: int) -> int:
    """Deterministic nonce per RFC 6979 with HMAC-SHA256."""
    x = priv.to_bytes(32, "big")
    h1 = digest
    v = b"\x01" * 32
    k = b"\x00" * 32
    k = hmac.new(k, v + b"\x00" + x + h1, hashlib.sha256).digest()
    v = hmac.new(k, v, hashlib.sha256).digest()
    k = hmac.new(k, v + b"\x01" + x + h1, hashlib.sha256).digest()
    v = hmac.new(k, v, hashlib.sha256).digest()
    while True:
        v = hmac.new(k, v, hashlib.sha256).digest()
        candidate = int.from_bytes(v, "big")
        if 1 <= candidate < _N:
            return candidate
        k = hmac.new(k, v + b"\x00", hashlib.sha256).digest()
        v = hmac.new(k, v, hashlib.sha256).digest()


def _sign_digest(digest: bytes, priv: int):
    """ECDSA over a 32-byte digest → (r, s, recovery_id), low-S normalized."""
    z = int.from_bytes(digest, "big")
    while True:
        k = _rfc6979_k(digest, priv)
        px, py = _point_mul(k, (_GX, _GY))
        r = px % _N
        if r == 0:
            digest = hashlib.sha256(digest).digest()  # unreachable in practice
            continue
        s = _inv(k, _N) * (z + r * priv) % _N
        if s == 0:
            digest = hashlib.sha256(digest).digest()
            continue
        recid = (py & 1) | (2 if px >= _N else 0)
        if s > _N // 2:  # the server rejects high-S outright
            s = _N - s
            recid ^= 1
        return r, s, recid


class Wallet:
    """A secp256k1 keypair that can do EIP-191 personal_sign."""

    def __init__(self, priv: int | None = None):
        self.priv = priv if priv is not None else (secrets.randbelow(_N - 1) + 1)
        px, py = _point_mul(self.priv, (_GX, _GY))
        self.pubkey_bytes = b"\x04" + px.to_bytes(32, "big") + py.to_bytes(32, "big")
        self.address = "0x" + keccak256(self.pubkey_bytes[1:])[-20:].hex()

    def personal_sign(self, message: str) -> str:
        """EIP-191: sign keccak256("\\x19Ethereum Signed Message:\\n" + len + msg).

        Returns 0x + r(32) + s(32) + v(1) hex, v in {27, 28} — exactly what
        POST /api/auth/login expects. The length prefix counts UTF-8 bytes,
        not characters.
        """
        raw = message.encode("utf-8")
        prefixed = b"\x19Ethereum Signed Message:\n" + str(len(raw)).encode() + raw
        digest = keccak256(prefixed)
        r, s, recid = _sign_digest(digest, self.priv)
        return "0x" + r.to_bytes(32, "big").hex() + s.to_bytes(32, "big").hex() + bytes([27 + recid]).hex()


def _self_test():
    assert keccak256(b"").hex() == "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    assert keccak256(b"abc").hex() == "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45"
    # The canonical address of private key 1.
    assert Wallet(1).address == "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"


_self_test()
