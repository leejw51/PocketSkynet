//! `msgHash` for plaintext messages (PROTOCOL.md §13): lowercase hex
//! SHA-256 of the **trimmed** content — plain SHA-256, never Keccak.
//!
//! The server (Rust) applies `str::trim()` before storing, so the client must
//! trim with the same Unicode-whitespace semantics or persist a hash that
//! never matches the persisted content.

const std = @import("std");
const hexmod = @import("hex.zig");

const Sha256 = std.crypto.hash.sha2.Sha256;

/// Code points Rust's `char::is_whitespace` (Unicode `White_Space`) accepts.
fn isUnicodeWhitespace(cp: u21) bool {
    return switch (cp) {
        0x09...0x0d, 0x20 => true, // ASCII: tab, LF, VT, FF, CR, space
        0x85 => true, // NEL
        0xa0 => true, // NBSP
        0x1680 => true, // ogham space mark
        0x2000...0x200a => true, // en quad .. hair space
        0x2028, 0x2029 => true, // line / paragraph separator
        0x202f => true, // narrow NBSP
        0x205f => true, // medium mathematical space
        0x3000 => true, // ideographic space
        else => false,
    };
}

/// Unicode-whitespace trim matching Rust `str::trim()` / JS `String.trim()`.
/// Invalid UTF-8 sequences are treated as non-whitespace and kept.
pub fn trim(s: []const u8) []const u8 {
    var start: usize = 0;
    while (start < s.len) {
        const len = std.unicode.utf8ByteSequenceLength(s[start]) catch break;
        if (start + len > s.len) break;
        const cp = std.unicode.utf8Decode(s[start .. start + len]) catch break;
        if (!isUnicodeWhitespace(cp)) break;
        start += len;
    }
    var end: usize = s.len;
    while (end > start) {
        // Walk back to the start of the previous code point.
        var cp_start = end - 1;
        while (cp_start > start and (s[cp_start] & 0xc0) == 0x80) cp_start -= 1;
        const len = std.unicode.utf8ByteSequenceLength(s[cp_start]) catch break;
        if (cp_start + len != end) break; // malformed tail: keep it
        const cp = std.unicode.utf8Decode(s[cp_start..end]) catch break;
        if (!isUnicodeWhitespace(cp)) break;
        end = cp_start;
    }
    return s[start..end];
}

/// SHA-256 of the trimmed content, as 64 lowercase hex chars.
pub fn msgHashPlaintext(content: []const u8) [64]u8 {
    var digest: [32]u8 = undefined;
    Sha256.hash(trim(content), &digest, .{});
    var out: [64]u8 = undefined;
    hexmod.encode(&out, &digest);
    return out;
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "trim removes ASCII whitespace on both ends" {
    try testing.expectEqualStrings("hello", trim("  hello \n"));
    try testing.expectEqualStrings("a b", trim("\t\r\na b\x0b\x0c"));
    try testing.expectEqualStrings("", trim("   \n\t  "));
    try testing.expectEqualStrings("x", trim("x"));
    try testing.expectEqualStrings("", trim(""));
}

test "trim removes Unicode whitespace like the server does" {
    // NBSP (U+00A0), ideographic space (U+3000), narrow NBSP (U+202F)
    try testing.expectEqualStrings("core", trim("\u{00a0}core\u{3000}"));
    try testing.expectEqualStrings("core", trim("\u{202f}\u{2028}core\u{2009}"));
    // ZWSP (U+200B) is NOT White_Space; it must survive.
    try testing.expectEqualStrings("\u{200b}core\u{200b}", trim("\u{200b}core\u{200b}"));
}

test "trim keeps interior whitespace and non-whitespace unicode" {
    try testing.expectEqualStrings("a  b", trim(" a  b "));
    try testing.expectEqualStrings("한글 🍓", trim("  한글 🍓\n"));
}

test "msgHashPlaintext hashes the trimmed bytes" {
    // sha256("hello"), i.e. of "  hello \n" after trimming.
    try testing.expectEqualStrings(
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        &msgHashPlaintext("  hello \n"),
    );
    try testing.expectEqualStrings(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        &msgHashPlaintext("abc"),
    );
}
