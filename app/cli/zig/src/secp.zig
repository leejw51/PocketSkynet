//! secp256k1 signing for the PocketSkynet protocol, in pure Zig.
//!
//! Built on `std.crypto.ecc.Secp256k1` (Jacobian group arithmetic and mod-n
//! scalar arithmetic from the standard library) with the Ethereum-specific
//! parts implemented here: RFC 6979 deterministic nonces (HMAC-SHA256),
//! low-S normalization, the recovery id, public-key recovery, address
//! derivation (Keccak-256 of X‖Y without the 0x04 byte) and EIP-55 checksums.
//!
//! `std.crypto.ecdsa` was checked and deliberately not used: its signer is
//! generic over (curve, hash), hashes the message itself and never exposes a
//! recovery id, so it cannot reproduce Ethereum's 65-byte `r‖s‖v` signatures.
//! Everything below is pinned byte-for-byte by the `eip191[]` vectors in
//! `app/core/tests/vectors/protocol-v1.json`.

const std = @import("std");
const hexmod = @import("hex.zig");

const Secp256k1 = std.crypto.ecc.Secp256k1;
const scalar = Secp256k1.scalar;
const Keccak256 = std.crypto.hash.sha3.Keccak256;
const HmacSha256 = std.crypto.auth.hmac.sha2.HmacSha256;

/// The group order n, big-endian.
pub const group_order: [32]u8 = .{
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b,
    0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
};

/// n/2 (floor), big-endian. `s` values above this are malleable high-S forms.
pub const half_group_order: [32]u8 = .{
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d,
    0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
};

pub const Error = error{
    /// Private key is zero or >= n (or hex-malformed at the caller).
    InvalidPrivateKey,
    /// Signature r/s out of range, off-curve recovery point, or bad v.
    InvalidSignature,
    /// Public key bytes do not name a curve point.
    InvalidPublicKey,
};

fn isZero32(b: [32]u8) bool {
    return std.mem.allEqual(u8, &b, 0);
}

/// big-endian unsigned comparison: a >= b
fn geq(a: [32]u8, b: [32]u8) bool {
    return std.mem.order(u8, &a, &b) != .lt;
}

/// Reduce a 32-byte big-endian integer mod n.
fn reduceModN(b: [32]u8) [32]u8 {
    var wide: [48]u8 = @splat(0);
    @memcpy(wide[16..], &b);
    return scalar.reduce48(wide, .big);
}

pub const PrivateKey = struct {
    bytes: [32]u8,

    /// Reject 0 and anything >= n — both on import, per PROTOCOL.md §3.4.
    pub fn fromBytes(bytes: [32]u8) Error!PrivateKey {
        if (isZero32(bytes)) return error.InvalidPrivateKey;
        if (geq(bytes, group_order)) return error.InvalidPrivateKey;
        return .{ .bytes = bytes };
    }

    /// Accepts 64 hex chars with an optional `0x` prefix, either case.
    pub fn fromHex(s: []const u8) Error!PrivateKey {
        const raw = hexmod.decodeFixed(32, s) catch return error.InvalidPrivateKey;
        return fromBytes(raw);
    }

    /// Uncompressed SEC1 public key: `04 ‖ X ‖ Y`, 65 bytes.
    pub fn publicKeyUncompressed(self: PrivateKey) [65]u8 {
        const p = Secp256k1.basePoint.mul(self.bytes, .big) catch unreachable;
        return p.toUncompressedSec1();
    }

    /// 20-byte Ethereum address: keccak256(X ‖ Y)[12..32] — the 0x04 SEC1
    /// byte is dropped before hashing (the classic porting bug).
    pub fn address(self: PrivateKey) [20]u8 {
        return addressFromPublicKey(self.publicKeyUncompressed());
    }

    /// `"0x"` + 40 lowercase hex chars.
    pub fn addressHex(self: PrivateKey) [42]u8 {
        return addressToHex(self.address());
    }
};

pub fn addressFromPublicKey(sec1_uncompressed: [65]u8) [20]u8 {
    var digest: [32]u8 = undefined;
    Keccak256.hash(sec1_uncompressed[1..], &digest, .{});
    var out: [20]u8 = undefined;
    @memcpy(&out, digest[12..32]);
    return out;
}

pub fn addressToHex(addr: [20]u8) [42]u8 {
    var out: [42]u8 = undefined;
    out[0] = '0';
    out[1] = 'x';
    hexmod.encode(out[2..], &addr);
    return out;
}

/// EIP-55 checksummed display form: uppercase hex letter i iff nibble i of
/// keccak256(lower40) >= 8. Display-only; the wire always carries lowercase.
pub fn eip55Checksum(addr: [20]u8) [42]u8 {
    var lower = addressToHex(addr);
    var digest: [32]u8 = undefined;
    Keccak256.hash(lower[2..], &digest, .{});
    for (lower[2..], 0..) |*c, i| {
        const nib = if (i % 2 == 0) digest[i / 2] >> 4 else digest[i / 2] & 0x0f;
        if (c.* >= 'a' and c.* <= 'f' and nib >= 8) c.* -= 'a' - 'A';
    }
    return lower;
}

/// An Ethereum-style recoverable signature: `r ‖ s ‖ v`, low-S, v ∈ {27, 28}.
pub const Signature = struct {
    r: [32]u8,
    s: [32]u8,
    v: u8,

    pub fn toBytes(self: Signature) [65]u8 {
        var out: [65]u8 = undefined;
        @memcpy(out[0..32], &self.r);
        @memcpy(out[32..64], &self.s);
        out[64] = self.v;
        return out;
    }

    /// Wire form: `"0x"` + 130 lowercase hex chars.
    pub fn toWireHex(self: Signature) [132]u8 {
        var out: [132]u8 = undefined;
        out[0] = '0';
        out[1] = 'x';
        hexmod.encode(out[2..], &self.toBytes());
        return out;
    }

    /// Parse `0x`-prefixed (or bare) 130-hex-char wire form. Emits 27/28 but
    /// tolerates v of 0/1 on input, per PROTOCOL.md §4.
    pub fn fromWireHex(s: []const u8) Error!Signature {
        const raw = hexmod.decodeFixed(65, s) catch return error.InvalidSignature;
        var sig = Signature{ .r = undefined, .s = undefined, .v = raw[64] };
        @memcpy(&sig.r, raw[0..32]);
        @memcpy(&sig.s, raw[32..64]);
        if (sig.v == 0 or sig.v == 1) sig.v += 27;
        if (sig.v != 27 and sig.v != 28) return error.InvalidSignature;
        return sig;
    }
};

/// RFC 6979 §3.2 deterministic nonce generator for qlen = hlen = 256.
const Rfc6979 = struct {
    k: [32]u8,
    v: [32]u8,

    fn init(private_key: [32]u8, digest: [32]u8) Rfc6979 {
        // bits2octets(h1) = int(h1) mod n, as 32 octets.
        const h1o = reduceModN(digest);
        var self = Rfc6979{ .k = @splat(0x00), .v = @splat(0x01) };
        self.round(0x00, private_key, h1o);
        self.round(0x01, private_key, h1o);
        return self;
    }

    fn round(self: *Rfc6979, tag: u8, x: [32]u8, h1o: [32]u8) void {
        var mac = HmacSha256.init(&self.k);
        mac.update(&self.v);
        mac.update(&.{tag});
        mac.update(&x);
        mac.update(&h1o);
        mac.final(&self.k);
        HmacSha256.create(&self.v, &self.v, &self.k);
    }

    /// Next candidate nonce. Callers reject invalid candidates and call again.
    fn next(self: *Rfc6979) [32]u8 {
        HmacSha256.create(&self.v, &self.v, &self.k);
        return self.v;
    }

    /// RFC 6979 step h.3: re-key after a rejected candidate.
    fn reject(self: *Rfc6979) void {
        var mac = HmacSha256.init(&self.k);
        mac.update(&self.v);
        mac.update(&.{0x00});
        mac.final(&self.k);
        HmacSha256.create(&self.v, &self.v, &self.k);
    }
};

/// Sign a 32-byte digest: RFC 6979 nonce, low-S normalized, v = recid + 27.
/// Deterministic — the same (key, digest) always yields the same 65 bytes.
pub fn signDigest(digest: [32]u8, key: PrivateKey) Signature {
    const z = reduceModN(digest);
    var gen = Rfc6979.init(key.bytes, digest);

    while (true) {
        const k_bytes = gen.next();
        if (isZero32(k_bytes) or geq(k_bytes, group_order)) {
            gen.reject();
            continue;
        }

        // R = k·G. k is nonzero and < n, so the multiply cannot hit identity.
        const big_r = Secp256k1.basePoint.mul(k_bytes, .big) catch {
            gen.reject();
            continue;
        };
        const affine = big_r.affineCoordinates();
        const x_bytes = affine.x.toBytes(.big);
        const r = reduceModN(x_bytes);
        if (isZero32(r)) {
            gen.reject();
            continue;
        }

        // s = k⁻¹ · (z + r·d) mod n
        const rd_plus_z = scalar.mulAdd(r, key.bytes, z, .big) catch unreachable;
        const k_scalar = scalar.Scalar.fromBytes(k_bytes, .big) catch unreachable;
        const k_inv = k_scalar.invert().toBytes(.big);
        var s = scalar.mul(rd_plus_z, k_inv, .big) catch unreachable;
        if (isZero32(s)) {
            gen.reject();
            continue;
        }

        // R.x >= n would need recid 2/3 (v = 29/30), which Ethereum tooling
        // (ethers, and this server) does not accept. Probability ~2^-127;
        // rejecting the nonce keeps the derivation deterministic and keeps
        // the v ∈ {27, 28} invariant unconditional.
        if (geq(x_bytes, group_order)) {
            gen.reject();
            continue;
        }

        var recid: u8 = 0;
        if (affine.y.isOdd()) recid |= 1;

        // Low-S normalization flips the parity of the matching R.y.
        if (std.mem.order(u8, &s, &half_group_order) == .gt) {
            s = scalar.neg(s, .big) catch unreachable;
            recid ^= 1;
        }

        return .{ .r = r, .s = s, .v = recid + 27 };
    }
}

/// Recover the uncompressed public key from a digest and signature.
/// Rejects out-of-range r/s, high-S signatures (malleable; ethers v6 rejects
/// them server-side too) and points not on the curve.
pub fn recoverPublicKey(digest: [32]u8, sig: Signature) Error![65]u8 {
    if (isZero32(sig.r) or geq(sig.r, group_order)) return error.InvalidSignature;
    if (isZero32(sig.s) or geq(sig.s, group_order)) return error.InvalidSignature;
    if (std.mem.order(u8, &sig.s, &half_group_order) == .gt) return error.InvalidSignature;
    if (sig.v != 27 and sig.v != 28) return error.InvalidSignature;
    const recid: u8 = sig.v - 27;

    // R.x = r (never r + n: v ∈ {27, 28} means the high-x form was not used).
    var compressed: [33]u8 = undefined;
    compressed[0] = 0x02 + (recid & 1);
    @memcpy(compressed[1..], &sig.r);
    const point_r = Secp256k1.fromSec1(&compressed) catch return error.InvalidSignature;

    // Q = r⁻¹·(s·R − z·G)
    const z = reduceModN(digest);
    const r_scalar = scalar.Scalar.fromBytes(sig.r, .big) catch unreachable;
    const r_inv = r_scalar.invert().toBytes(.big);
    const scalar_g = scalar.mul(scalar.neg(z, .big) catch unreachable, r_inv, .big) catch unreachable;
    const scalar_r = scalar.mul(sig.s, r_inv, .big) catch unreachable;
    const q = Secp256k1.mulDoubleBasePublic(Secp256k1.basePoint, scalar_g, point_r, scalar_r, .big) catch
        return error.InvalidSignature;
    return q.toUncompressedSec1();
}

/// Recover the signer's address (lowercase comparison form).
pub fn recoverAddress(digest: [32]u8, sig: Signature) Error![20]u8 {
    return addressFromPublicKey(try recoverPublicKey(digest, sig));
}

/// ECDH: X coordinate of d·P, 32 bytes big-endian, zero-left-padded, unhashed
/// (PROTOCOL.md §9 / trap 7).
pub fn ecdhSharedX(key: PrivateKey, peer_uncompressed: [65]u8) Error![32]u8 {
    const peer = Secp256k1.fromSec1(&peer_uncompressed) catch return error.InvalidPublicKey;
    const shared = peer.mul(key.bytes, .big) catch return error.InvalidPublicKey;
    return shared.affineCoordinates().x.toBytes(.big);
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "private key 0 is rejected" {
    const zero: [32]u8 = @splat(0);
    try testing.expectError(error.InvalidPrivateKey, PrivateKey.fromBytes(zero));
}

test "private key n and above are rejected" {
    try testing.expectError(error.InvalidPrivateKey, PrivateKey.fromBytes(group_order));
    const max: [32]u8 = @splat(0xff);
    try testing.expectError(error.InvalidPrivateKey, PrivateKey.fromBytes(max));
}

test "private key n-1 is accepted" {
    var n_minus_1 = group_order;
    n_minus_1[31] -= 1;
    const key = try PrivateKey.fromBytes(n_minus_1);
    // Its public key is the negation of G: same X, odd/even Y flipped.
    const pub_key = key.publicKeyUncompressed();
    const g = Secp256k1.basePoint.toUncompressedSec1();
    try testing.expectEqualSlices(u8, g[1..33], pub_key[1..33]);
}

test "fromHex accepts optional 0x and rejects malformed input" {
    const a = try PrivateKey.fromHex("0x0000000000000000000000000000000000000000000000000000000000000001");
    const b = try PrivateKey.fromHex("0000000000000000000000000000000000000000000000000000000000000001");
    try testing.expectEqualSlices(u8, &a.bytes, &b.bytes);
    try testing.expectError(error.InvalidPrivateKey, PrivateKey.fromHex("0x01"));
    try testing.expectError(error.InvalidPrivateKey, PrivateKey.fromHex("zz"));
}

test "RFC 6979 signatures are deterministic" {
    const key = try PrivateKey.fromHex("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    var digest: [32]u8 = undefined;
    Keccak256.hash("determinism check", &digest, .{});
    const sig1 = signDigest(digest, key);
    const sig2 = signDigest(digest, key);
    try testing.expectEqualSlices(u8, &sig1.toBytes(), &sig2.toBytes());
}

test "signatures are low-S with v in {27, 28} and recover to the signer" {
    const key = try PrivateKey.fromHex("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    // A spread of messages makes it overwhelmingly likely both v values and
    // (pre-normalization) high-S cases appear.
    var msg_buf: [32]u8 = undefined;
    for (0..16) |i| {
        const msg = std.fmt.bufPrint(&msg_buf, "message {d}", .{i}) catch unreachable;
        var digest: [32]u8 = undefined;
        Keccak256.hash(msg, &digest, .{});
        const sig = signDigest(digest, key);
        try testing.expect(sig.v == 27 or sig.v == 28);
        try testing.expect(std.mem.order(u8, &sig.s, &half_group_order) != .gt);
        const recovered = try recoverAddress(digest, sig);
        try testing.expectEqualSlices(u8, &key.address(), &recovered);
    }
}

test "recover rejects a high-S signature" {
    const key = try PrivateKey.fromHex("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    var digest: [32]u8 = undefined;
    Keccak256.hash("high-s", &digest, .{});
    var sig = signDigest(digest, key);
    // Re-malleate: s' = n - s, flip v.
    sig.s = scalar.neg(sig.s, .big) catch unreachable;
    sig.v = if (sig.v == 27) 28 else 27;
    try testing.expectError(error.InvalidSignature, recoverAddress(digest, sig));
}

test "recover rejects zero and out-of-range r/s" {
    var digest: [32]u8 = undefined;
    Keccak256.hash("range", &digest, .{});
    const good = signDigest(
        digest,
        try PrivateKey.fromHex("0x" ++ "11" ** 32),
    );
    var bad = good;
    bad.r = @splat(0);
    try testing.expectError(error.InvalidSignature, recoverPublicKey(digest, bad));
    bad = good;
    bad.r = group_order;
    try testing.expectError(error.InvalidSignature, recoverPublicKey(digest, bad));
    bad = good;
    bad.s = @splat(0);
    try testing.expectError(error.InvalidSignature, recoverPublicKey(digest, bad));
    bad = good;
    bad.v = 29;
    try testing.expectError(error.InvalidSignature, recoverPublicKey(digest, bad));
}

test "wire hex round-trips and tolerates v of 0/1 on input" {
    const key = try PrivateKey.fromHex("0x" ++ "22" ** 32);
    var digest: [32]u8 = undefined;
    Keccak256.hash("wire", &digest, .{});
    const sig = signDigest(digest, key);
    const wire = sig.toWireHex();
    try testing.expect(wire[0] == '0' and wire[1] == 'x');
    const parsed = try Signature.fromWireHex(&wire);
    try testing.expectEqualSlices(u8, &sig.toBytes(), &parsed.toBytes());

    // v = 0/1 tolerated on input, normalized to 27/28.
    var legacy = wire;
    legacy[130] = '0';
    legacy[131] = if (sig.v == 27) '0' else '1';
    const parsed_legacy = try Signature.fromWireHex(&legacy);
    try testing.expectEqual(sig.v, parsed_legacy.v);

    // v = 2 is not a recovery id.
    var bad = wire;
    bad[130] = '0';
    bad[131] = '2';
    try testing.expectError(error.InvalidSignature, Signature.fromWireHex(&bad));
}
