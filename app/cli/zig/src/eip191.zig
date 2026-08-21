//! EIP-191 `personal_sign` (PROTOCOL.md §4).
//!
//! digest = keccak256(0x19 ‖ "Ethereum Signed Message:\n" ‖ decimal(byte_len) ‖ utf8(msg))
//!
//! The length is the UTF-8 **byte** length, never the character count, and
//! std's `Keccak256` is original Keccak (0x01 padding), not NIST SHA3-256
//! (0x06) — both facts are pinned by vectors.

const std = @import("std");
const secp = @import("secp.zig");

const Keccak256 = std.crypto.hash.sha3.Keccak256;

pub const prefix = "\x19Ethereum Signed Message:\n";

/// The EIP-191 digest of `message`.
pub fn digest(message: []const u8) [32]u8 {
    var st = Keccak256.init(.{});
    st.update(prefix);
    var len_buf: [20]u8 = undefined;
    const len_str = std.fmt.bufPrint(&len_buf, "{d}", .{message.len}) catch unreachable;
    st.update(len_str);
    st.update(message);
    var out: [32]u8 = undefined;
    st.final(&out);
    return out;
}

/// Sign `message` verbatim with `personal_sign` semantics: RFC 6979
/// deterministic nonce, low-S, v ∈ {27, 28}.
pub fn personalSign(message: []const u8, key: secp.PrivateKey) secp.Signature {
    return secp.signDigest(digest(message), key);
}

/// Sign and return the wire form: `"0x"` + 130 lowercase hex chars.
pub fn personalSignHex(message: []const u8, key: secp.PrivateKey) [132]u8 {
    return personalSign(message, key).toWireHex();
}

/// Verify by recovery: the recovered address must equal `expected_address`
/// (20 raw bytes; comparison is byte-wise, i.e. lowercase semantics).
pub fn verify(message: []const u8, sig: secp.Signature, expected_address: [20]u8) bool {
    const recovered = secp.recoverAddress(digest(message), sig) catch return false;
    return std.mem.eql(u8, &recovered, &expected_address);
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "digest prefixes the UTF-8 byte length, not the character count" {
    // "🍓 strawberry" is 12 code points but 15 UTF-8 bytes; the vector digest
    // for it is checked in tests/vectors.zig. Here: two messages of equal
    // character count but different byte count must not collide the way a
    // char-count implementation would make them.
    const ascii = "aa"; // 2 chars, 2 bytes
    const uni = "é"; // 1 char, 2 bytes
    try testing.expect(!std.mem.eql(u8, &digest(ascii), &digest(uni)));

    // Manual reconstruction for a unicode message.
    const msg = "🍓 strawberry";
    try testing.expectEqual(@as(usize, 15), msg.len);
    var manual = Keccak256.init(.{});
    manual.update("\x19Ethereum Signed Message:\n15");
    manual.update(msg);
    var expected: [32]u8 = undefined;
    manual.final(&expected);
    try testing.expectEqualSlices(u8, &expected, &digest(msg));
}

test "sign/verify round trip and tamper rejection" {
    const key = try secp.PrivateKey.fromHex("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    const msg = "hello world";
    const sig = personalSign(msg, key);
    try testing.expect(verify(msg, sig, key.address()));
    try testing.expect(!verify("hello world!", sig, key.address()));
    const other = try secp.PrivateKey.fromHex("0x" ++ "33" ** 32);
    try testing.expect(!verify(msg, sig, other.address()));
}
