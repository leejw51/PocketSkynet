//! Lowercase hex encoding/decoding helpers.
//!
//! The protocol mandates lowercase hex everywhere unless stated otherwise
//! (PROTOCOL.md §1), so encoding is always lowercase; decoding accepts both
//! cases and an optional `0x` prefix where the caller says so.

const std = @import("std");

pub const DecodeError = error{ InvalidHex, InvalidLength };

const alphabet = "0123456789abcdef";

/// Encode `bytes` as lowercase hex into `out`. `out.len` must be exactly
/// `bytes.len * 2`.
pub fn encode(out: []u8, bytes: []const u8) void {
    std.debug.assert(out.len == bytes.len * 2);
    for (bytes, 0..) |b, i| {
        out[i * 2] = alphabet[b >> 4];
        out[i * 2 + 1] = alphabet[b & 0x0f];
    }
}

/// Allocate and encode.
pub fn encodeAlloc(gpa: std.mem.Allocator, bytes: []const u8) std.mem.Allocator.Error![]u8 {
    const out = try gpa.alloc(u8, bytes.len * 2);
    encode(out, bytes);
    return out;
}

fn nibble(c: u8) DecodeError!u8 {
    return switch (c) {
        '0'...'9' => c - '0',
        'a'...'f' => c - 'a' + 10,
        'A'...'F' => c - 'A' + 10,
        else => error.InvalidHex,
    };
}

/// Decode hex (either case, no prefix) into `out`. `s.len` must be exactly
/// `out.len * 2`.
pub fn decode(out: []u8, s: []const u8) DecodeError!void {
    if (s.len != out.len * 2) return error.InvalidLength;
    for (out, 0..) |*b, i| {
        b.* = (try nibble(s[i * 2])) << 4 | try nibble(s[i * 2 + 1]);
    }
}

/// Strip an optional `0x`/`0X` prefix.
pub fn stripPrefix(s: []const u8) []const u8 {
    if (s.len >= 2 and s[0] == '0' and (s[1] == 'x' or s[1] == 'X')) return s[2..];
    return s;
}

/// Decode a fixed-size value, tolerating an optional `0x` prefix.
pub fn decodeFixed(comptime n: usize, s: []const u8) DecodeError![n]u8 {
    var out: [n]u8 = undefined;
    try decode(&out, stripPrefix(s));
    return out;
}

/// True when `s` is entirely lowercase hex of length `len`.
pub fn isLowerHex(s: []const u8, len: usize) bool {
    if (s.len != len) return false;
    for (s) |c| switch (c) {
        '0'...'9', 'a'...'f' => {},
        else => return false,
    };
    return true;
}

test "encode is lowercase" {
    var out: [8]u8 = undefined;
    encode(&out, &.{ 0xde, 0xad, 0xbe, 0xef });
    try std.testing.expectEqualStrings("deadbeef", &out);
}

test "decode accepts both cases and 0x prefix via decodeFixed" {
    const a = try decodeFixed(4, "DEADbeef");
    try std.testing.expectEqualSlices(u8, &.{ 0xde, 0xad, 0xbe, 0xef }, &a);
    const b = try decodeFixed(4, "0xdeadBEEF");
    try std.testing.expectEqualSlices(u8, &.{ 0xde, 0xad, 0xbe, 0xef }, &b);
}

test "decode rejects garbage and bad lengths" {
    try std.testing.expectError(error.InvalidHex, decodeFixed(2, "zzzz"));
    try std.testing.expectError(error.InvalidLength, decodeFixed(2, "abc"));
    try std.testing.expectError(error.InvalidLength, decodeFixed(2, "abcdef"));
}

test "isLowerHex" {
    try std.testing.expect(isLowerHex("00ff", 4));
    try std.testing.expect(!isLowerHex("00FF", 4));
    try std.testing.expect(!isLowerHex("00ff", 6));
    try std.testing.expect(!isLowerHex("00fg", 4));
}
