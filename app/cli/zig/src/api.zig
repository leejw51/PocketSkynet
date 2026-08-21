//! Typed API client for the PocketSkynet server (app/docs/API.md).
//!
//! Wire rules honored here:
//! - all body fields are camelCase; absent optionals are omitted entirely,
//!   never sent as `null` (API.md §1.4);
//! - the login challenge message is signed **verbatim** (§6.2.1);
//! - `challengeId` is required and burned by success AND failure, so every
//!   retry fetches a fresh challenge (§6.2.2 step 1);
//! - plaintext `msgHash` is SHA-256 of the trimmed content (§13).

const std = @import("std");
const transport = @import("transport.zig");
const secp = @import("secp.zig");
const eip191 = @import("eip191.zig");
const msghash = @import("msghash.zig");

const Keccak256 = std.crypto.hash.sha3.Keccak256;

pub const Error = error{
    /// Non-2xx status; details in `Client.last_status` / `Client.last_body`.
    Api,
    /// 2xx but the body did not parse as the expected shape.
    UnexpectedBody,
    /// Room/message id contains characters outside the wire format.
    InvalidId,
    OutOfMemory,
} || transport.Error;

// -- response shapes --------------------------------------------------------

pub const User = struct {
    walletAddress: []const u8 = "",
    username: []const u8 = "",
    publicKey: ?[]const u8 = null,
    publicKeySig: ?[]const u8 = null,
    profileImage: ?[]const u8 = null,
    createdAt: ?[]const u8 = null,
    updatedAt: ?[]const u8 = null,
};

pub const Room = struct {
    id: []const u8,
    name: []const u8 = "",
    description: ?[]const u8 = null,
    kind: ?[]const u8 = null,
    currentKeyVersion: i64 = 1,
    keyRotationPending: bool = false,
    createdAt: ?[]const u8 = null,
    memberCount: ?i64 = null,
    hasEncryption: ?bool = null,
    unreadCount: ?i64 = null,
    lastReadSerial: ?i64 = null,
};

pub const Message = struct {
    id: []const u8,
    roomId: []const u8 = "",
    senderAddress: []const u8 = "",
    content: []const u8 = "",
    msgHash: []const u8 = "",
    messageTimestamp: i64 = 0,
    msgType: []const u8 = "add",
    msgSerial: i64 = 0,
    isDeleted: bool = false,
    editedAt: ?[]const u8 = null,
    createdAt: ?[]const u8 = null,
    isEncrypted: bool = false,
    iv: ?[]const u8 = null,
    hmac: ?[]const u8 = null,
    encVer: ?i64 = null,
    keyVersion: ?i64 = null,
    txHash: ?[]const u8 = null,
    targetMessageId: ?[]const u8 = null,
    emoticonCode: ?[]const u8 = null,
    replyCount: ?i64 = null,
    lastReplyAt: ?i64 = null,
    sender: ?User = null,
};

pub const Health = struct {
    status: []const u8,
    uptime: ?i64 = null,
};

pub const Challenge = struct {
    challengeId: []const u8,
    message: []const u8,
    expiresAt: ?[]const u8 = null,
};

pub const LoginResponse = struct {
    user: User,
    token: []const u8,
    fruitnationWallet: ?[]const u8 = null,
    encryptionSalt: ?[]const u8 = null,
};

/// All three error-envelope shapes (API.md §1.5) parse into this one struct:
/// `{message}`, `{message, errors[]}`, `{code, message, currentKeyVersion}`.
pub const ErrorEnvelope = struct {
    message: ?[]const u8 = null,
    code: ?[]const u8 = null,
    errors: ?[]const []const u8 = null,
    currentKeyVersion: ?i64 = null,
    /// `GET /api/health` failures say `{"status": "unavailable"}`.
    status: ?[]const u8 = null,
};

pub fn parseErrorEnvelope(gpa: std.mem.Allocator, body: []const u8) ?std.json.Parsed(ErrorEnvelope) {
    return std.json.parseFromSlice(ErrorEnvelope, gpa, body, .{
        .ignore_unknown_fields = true,
    }) catch null;
}

// -- request-body builders (pure, unit-tested) ------------------------------

const stringify_opts: std.json.Stringify.Options = .{
    // Absent optionals are OMITTED, matching the reference client; the
    // username-omitted/username-present pair is pinned by unit tests.
    .emit_null_optional_fields = false,
};

pub fn buildChallengeBody(gpa: std.mem.Allocator, wallet_address: []const u8) std.mem.Allocator.Error![]u8 {
    return std.json.Stringify.valueAlloc(gpa, .{ .walletAddress = wallet_address }, stringify_opts);
}

pub fn buildLoginBody(
    gpa: std.mem.Allocator,
    wallet_address: []const u8,
    challenge_id: []const u8,
    signature: []const u8,
    username: ?[]const u8,
) std.mem.Allocator.Error![]u8 {
    const Body = struct {
        walletAddress: []const u8,
        challengeId: []const u8,
        signature: []const u8,
        username: ?[]const u8 = null,
    };
    return std.json.Stringify.valueAlloc(gpa, Body{
        .walletAddress = wallet_address,
        .challengeId = challenge_id,
        .signature = signature,
        .username = username,
    }, stringify_opts);
}

pub fn buildCreateRoomBody(gpa: std.mem.Allocator, name: []const u8, description: ?[]const u8) std.mem.Allocator.Error![]u8 {
    const Body = struct {
        name: []const u8,
        description: ?[]const u8 = null,
    };
    return std.json.Stringify.valueAlloc(gpa, Body{ .name = name, .description = description }, stringify_opts);
}

pub fn buildSendBody(gpa: std.mem.Allocator, content: []const u8, msg_hash: []const u8) std.mem.Allocator.Error![]u8 {
    return std.json.Stringify.valueAlloc(gpa, .{
        .content = content,
        .msgHash = msg_hash,
    }, stringify_opts);
}

/// Deterministic fallback username for first-time logins:
/// "zig" + 12 hex chars of keccak256(lowercase 0x-address).
pub fn fallbackUsername(address_hex: [42]u8) [15]u8 {
    var digest: [32]u8 = undefined;
    Keccak256.hash(&address_hex, &digest, .{});
    var out: [15]u8 = undefined;
    out[0] = 'z';
    out[1] = 'i';
    out[2] = 'g';
    const alphabet = "0123456789abcdef";
    for (digest[0..6], 0..) |b, i| {
        out[3 + i * 2] = alphabet[b >> 4];
        out[4 + i * 2] = alphabet[b & 0x0f];
    }
    return out;
}

/// Room ids are `[a-zA-Z0-9_.-]{10,100}` on the wire (API.md §3.1); anything
/// else must never reach URL construction.
pub fn isValidRoomId(id: []const u8) bool {
    if (id.len < 10 or id.len > 100) return false;
    for (id) |c| switch (c) {
        'a'...'z', 'A'...'Z', '0'...'9', '_', '.', '-' => {},
        else => return false,
    };
    return true;
}

// -- the client -------------------------------------------------------------

pub const Client = struct {
    gpa: std.mem.Allocator,
    t: *transport.Transport,
    base_url: []const u8,
    /// Owned JWT after a successful login.
    token: ?[]u8 = null,
    /// Status and raw body of the last non-2xx response (owned).
    last_status: u16 = 0,
    last_body: ?[]u8 = null,

    pub fn init(gpa: std.mem.Allocator, t: *transport.Transport, base_url: []const u8) Client {
        return .{ .gpa = gpa, .t = t, .base_url = base_url };
    }

    pub fn deinit(self: *Client) void {
        if (self.token) |tok| self.gpa.free(tok);
        if (self.last_body) |b| self.gpa.free(b);
        self.* = undefined;
    }

    pub fn setToken(self: *Client, token: []const u8) Error!void {
        const copy = try self.gpa.dupe(u8, token);
        if (self.token) |old| self.gpa.free(old);
        self.token = copy;
    }

    fn recordFailure(self: *Client, status: u16, body: []const u8) Error!void {
        const copy = try self.gpa.dupe(u8, body);
        if (self.last_body) |old| self.gpa.free(old);
        self.last_body = copy;
        self.last_status = status;
    }

    /// Human-readable message of the last failure ("HTTP 403: Access denied").
    /// Caller frees.
    pub fn lastFailureText(self: *Client, gpa: std.mem.Allocator) std.mem.Allocator.Error![]u8 {
        const body = self.last_body orelse
            return std.fmt.allocPrint(gpa, "HTTP {d}", .{self.last_status});
        if (parseErrorEnvelope(gpa, body)) |parsed| {
            defer parsed.deinit();
            const env = parsed.value;
            if (env.errors) |errs| if (errs.len > 0) {
                return std.fmt.allocPrint(gpa, "HTTP {d}: {s} ({s})", .{
                    self.last_status,
                    env.message orelse "Validation failed",
                    errs[0],
                });
            };
            if (env.message) |m| {
                if (env.code) |c| {
                    return std.fmt.allocPrint(gpa, "HTTP {d}: [{s}] {s}", .{ self.last_status, c, m });
                }
                return std.fmt.allocPrint(gpa, "HTTP {d}: {s}", .{ self.last_status, m });
            }
            if (env.status) |s| {
                return std.fmt.allocPrint(gpa, "HTTP {d}: status {s}", .{ self.last_status, s });
            }
        }
        return std.fmt.allocPrint(gpa, "HTTP {d}: {s}", .{ self.last_status, body });
    }

    fn requestParsed(
        self: *Client,
        comptime T: type,
        method: transport.Method,
        path: []const u8,
        body: ?[]const u8,
        authed: bool,
    ) Error!std.json.Parsed(T) {
        const url = try std.fmt.allocPrint(self.gpa, "{s}{s}", .{ self.base_url, path });
        defer self.gpa.free(url);
        const bearer: ?[]const u8 = if (authed) self.token else null;
        var resp = try self.t.request(method, url, body, bearer);
        defer resp.deinit(self.gpa);
        if (resp.status < 200 or resp.status > 299) {
            try self.recordFailure(resp.status, resp.body);
            return error.Api;
        }
        return std.json.parseFromSlice(T, self.gpa, resp.body, .{
            .ignore_unknown_fields = true,
            // The response buffer is freed on return; every parsed string
            // must be copied into the Parsed arena, not borrowed.
            .allocate = .alloc_always,
        }) catch error.UnexpectedBody;
    }

    // -- endpoints ----------------------------------------------------------

    pub fn health(self: *Client) Error!std.json.Parsed(Health) {
        return self.requestParsed(Health, .GET, "/api/health", null, false);
    }

    pub fn challenge(self: *Client, wallet_address: []const u8) Error!std.json.Parsed(Challenge) {
        const body = try buildChallengeBody(self.gpa, wallet_address);
        defer self.gpa.free(body);
        return self.requestParsed(Challenge, .POST, "/api/auth/challenge", body, false);
    }

    fn loginAttempt(
        self: *Client,
        key: secp.PrivateKey,
        username: ?[]const u8,
    ) Error!std.json.Parsed(LoginResponse) {
        const addr = key.addressHex();
        const ch = try self.challenge(&addr);
        defer ch.deinit();
        // Sign the challenge message VERBATIM — never reconstruct it.
        const sig = eip191.personalSignHex(ch.value.message, key);
        const body = try buildLoginBody(self.gpa, &addr, ch.value.challengeId, &sig, username);
        defer self.gpa.free(body);
        return self.requestParsed(LoginResponse, .POST, "/api/auth/login", body, false);
    }

    /// Full login flow. A challenge is burned by failure as well as success,
    /// so the first-time-login retry fetches a fresh one and supplies a
    /// generated username.
    pub fn login(self: *Client, key: secp.PrivateKey, username: ?[]const u8) Error!std.json.Parsed(LoginResponse) {
        const first = self.loginAttempt(key, username);
        if (first) |parsed| {
            // If storing the token fails (OOM), the parsed response would
            // otherwise leak its arena — free it before propagating.
            self.setToken(parsed.value.token) catch |err| {
                parsed.deinit();
                return err;
            };
            return parsed;
        } else |err| {
            if (err != error.Api or username != null) return err;
            if (!self.lastFailureNeedsUsername()) return err;
            const generated = fallbackUsername(key.addressHex());
            const second = try self.loginAttempt(key, &generated);
            self.setToken(second.value.token) catch |set_err| {
                second.deinit();
                return set_err;
            };
            return second;
        }
    }

    fn lastFailureNeedsUsername(self: *Client) bool {
        if (self.last_status != 400) return false;
        const body = self.last_body orelse return false;
        const parsed = parseErrorEnvelope(self.gpa, body) orelse return false;
        defer parsed.deinit();
        const msg = parsed.value.message orelse return false;
        return std.mem.find(u8, msg, "Username is required") != null;
    }

    pub fn listRooms(self: *Client) Error!std.json.Parsed([]Room) {
        return self.requestParsed([]Room, .GET, "/api/rooms", null, true);
    }

    pub fn createRoom(self: *Client, name: []const u8, description: ?[]const u8) Error!std.json.Parsed(Room) {
        const body = try buildCreateRoomBody(self.gpa, name, description);
        defer self.gpa.free(body);
        return self.requestParsed(Room, .POST, "/api/rooms", body, true);
    }

    /// Send plaintext: msgHash is computed here from the trimmed content.
    pub fn sendText(self: *Client, room_id: []const u8, text: []const u8) Error!std.json.Parsed(Message) {
        const hash = msghash.msgHashPlaintext(text);
        return self.sendRaw(room_id, text, &hash);
    }

    /// Raw send with a caller-supplied msgHash (integration tests use this to
    /// probe the server's msgHash validation).
    pub fn sendRaw(self: *Client, room_id: []const u8, content: []const u8, msg_hash: []const u8) Error!std.json.Parsed(Message) {
        if (!isValidRoomId(room_id)) return error.InvalidId;
        const body = try buildSendBody(self.gpa, content, msg_hash);
        defer self.gpa.free(body);
        const path = try std.fmt.allocPrint(self.gpa, "/api/rooms/{s}/messages", .{room_id});
        defer self.gpa.free(path);
        return self.requestParsed(Message, .POST, path, body, true);
    }

    pub fn listMessages(self: *Client, room_id: []const u8, limit: ?u32) Error!std.json.Parsed([]Message) {
        if (!isValidRoomId(room_id)) return error.InvalidId;
        const path = if (limit) |n|
            try std.fmt.allocPrint(self.gpa, "/api/rooms/{s}/messages?limit={d}", .{ room_id, n })
        else
            try std.fmt.allocPrint(self.gpa, "/api/rooms/{s}/messages", .{room_id});
        defer self.gpa.free(path);
        return self.requestParsed([]Message, .GET, path, null, true);
    }

    pub fn profile(self: *Client) Error!std.json.Parsed(User) {
        return self.requestParsed(User, .GET, "/api/auth/profile", null, true);
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "challenge body is camelCase" {
    const gpa = testing.allocator;
    const body = try buildChallengeBody(gpa, "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
    defer gpa.free(body);
    try testing.expectEqualStrings(
        "{\"walletAddress\":\"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266\"}",
        body,
    );
}

test "login body omits username when absent and includes it when present" {
    const gpa = testing.allocator;
    const omitted = try buildLoginBody(gpa, "0xabc", "cid-1", "0xdead", null);
    defer gpa.free(omitted);
    try testing.expectEqualStrings(
        "{\"walletAddress\":\"0xabc\",\"challengeId\":\"cid-1\",\"signature\":\"0xdead\"}",
        omitted,
    );
    try testing.expect(std.mem.find(u8, omitted, "username") == null);

    const present = try buildLoginBody(gpa, "0xabc", "cid-1", "0xdead", "alice");
    defer gpa.free(present);
    try testing.expectEqualStrings(
        "{\"walletAddress\":\"0xabc\",\"challengeId\":\"cid-1\",\"signature\":\"0xdead\",\"username\":\"alice\"}",
        present,
    );
}

test "create-room body omits an absent description" {
    const gpa = testing.allocator;
    const bare = try buildCreateRoomBody(gpa, "Team chat", null);
    defer gpa.free(bare);
    try testing.expectEqualStrings("{\"name\":\"Team chat\"}", bare);
    const with = try buildCreateRoomBody(gpa, "Team chat", "hello");
    defer gpa.free(with);
    try testing.expectEqualStrings("{\"name\":\"Team chat\",\"description\":\"hello\"}", with);
}

test "send body carries content and msgHash, JSON-escaping hostile text" {
    const gpa = testing.allocator;
    const evil = "\"; rm -rf ~\n`echo hi`$(x)";
    const body = try buildSendBody(gpa, evil, "ab" ** 32);
    defer gpa.free(body);
    // Round-trip: the parsed content must be byte-identical.
    const parsed = try std.json.parseFromSlice(struct {
        content: []const u8,
        msgHash: []const u8,
    }, gpa, body, .{});
    defer parsed.deinit();
    try testing.expectEqualStrings(evil, parsed.value.content);
    try testing.expectEqualStrings("ab" ** 32, parsed.value.msgHash);
}

test "unicode content round-trips through the send body" {
    const gpa = testing.allocator;
    const text = "한글 메시지 🍓🍊";
    const body = try buildSendBody(gpa, text, "cd" ** 32);
    defer gpa.free(body);
    const parsed = try std.json.parseFromSlice(struct {
        content: []const u8,
        msgHash: []const u8,
    }, gpa, body, .{});
    defer parsed.deinit();
    try testing.expectEqualStrings(text, parsed.value.content);
}

test "error envelope: all three shapes parse" {
    const gpa = testing.allocator;

    const plain = parseErrorEnvelope(gpa, "{\"message\":\"Access denied\"}").?;
    defer plain.deinit();
    try testing.expectEqualStrings("Access denied", plain.value.message.?);
    try testing.expect(plain.value.code == null);
    try testing.expect(plain.value.errors == null);

    const validation = parseErrorEnvelope(gpa,
        \\{"message":"Validation failed","errors":["roomId: Room ID contains invalid characters"]}
    ).?;
    defer validation.deinit();
    try testing.expectEqualStrings("Validation failed", validation.value.message.?);
    try testing.expectEqual(@as(usize, 1), validation.value.errors.?.len);
    try testing.expectEqualStrings(
        "roomId: Room ID contains invalid characters",
        validation.value.errors.?[0],
    );

    const coded = parseErrorEnvelope(gpa,
        \\{"code":"KEY_ROTATION_REQUIRED","message":"rotate first","currentKeyVersion":3}
    ).?;
    defer coded.deinit();
    try testing.expectEqualStrings("KEY_ROTATION_REQUIRED", coded.value.code.?);
    try testing.expectEqual(@as(i64, 3), coded.value.currentKeyVersion.?);
}

test "error envelope: unknown fields and non-envelope bodies do not crash" {
    const gpa = testing.allocator;
    const extra = parseErrorEnvelope(gpa, "{\"message\":\"x\",\"unexpected\":[1,2,{}]}").?;
    defer extra.deinit();
    try testing.expectEqualStrings("x", extra.value.message.?);
    try testing.expect(parseErrorEnvelope(gpa, "not json at all") == null);
    try testing.expect(parseErrorEnvelope(gpa, "[1,2,3]") == null);
}

test "User parses with explicit nulls (never-published keys)" {
    const gpa = testing.allocator;
    const parsed = try std.json.parseFromSlice(User, gpa,
        \\{"walletAddress":"0xabc","username":"alice","publicKey":null,
        \\ "publicKeySig":null,"profileImage":null,
        \\ "createdAt":"2025-06-11T14:39:06.000Z","updatedAt":null}
    , .{ .ignore_unknown_fields = true });
    defer parsed.deinit();
    try testing.expectEqualStrings("alice", parsed.value.username);
    try testing.expect(parsed.value.publicKey == null);
    try testing.expect(parsed.value.updatedAt == null);
    try testing.expectEqualStrings("2025-06-11T14:39:06.000Z", parsed.value.createdAt.?);
}

test "Message parses with nulls, absent optionals and an ignored sender blob" {
    const gpa = testing.allocator;
    const parsed = try std.json.parseFromSlice(Message, gpa,
        \\{"id":"msg_1749652746620_4cfe1c4c","roomId":"room_1","senderAddress":"0xabc",
        \\ "content":"Hello","msgHash":"aa","messageTimestamp":1749652746620,
        \\ "msgType":"add","msgSerial":1749652746620,"isDeleted":false,"editedAt":null,
        \\ "isEncrypted":false,"iv":null,"hmac":null,"encVer":1,"keyVersion":1,
        \\ "txHash":null,"targetMessageId":null,"emoticonCode":null,
        \\ "sender":{"walletAddress":"0xabc","username":"alice","futureField":123}}
    , .{ .ignore_unknown_fields = true });
    defer parsed.deinit();
    try testing.expectEqual(@as(i64, 1749652746620), parsed.value.msgSerial);
    try testing.expect(parsed.value.iv == null);
    try testing.expect(parsed.value.replyCount == null); // absent, not null
    try testing.expectEqualStrings("alice", parsed.value.sender.?.username);
}

test "RoomWithMembers parses as Room, ignoring the roster" {
    const gpa = testing.allocator;
    const parsed = try std.json.parseFromSlice(Room, gpa,
        \\{"id":"room_1749652739650_304e0eaf","name":"My Note","description":null,
        \\ "kind":"channel","currentKeyVersion":1,"keyRotationPending":false,
        \\ "createdAt":"2025-06-11T14:38:59.000Z","memberCount":1,
        \\ "members":[{"id":42,"roomId":"room_x","userAddress":"0xabc",
        \\   "joinedAt":"2025-06-11T14:39:00.000Z",
        \\   "user":{"walletAddress":"0xabc","username":"alice"}}],
        \\ "admins":[{"walletAddress":"0xabc","username":"alice"}],
        \\ "hasEncryption":false,"unreadCount":4,"lastReadSerial":0}
    , .{ .ignore_unknown_fields = true });
    defer parsed.deinit();
    try testing.expectEqualStrings("My Note", parsed.value.name);
    try testing.expectEqual(@as(i64, 4), parsed.value.unreadCount.?);
    try testing.expectEqual(@as(i64, 1), parsed.value.memberCount.?);
}

test "Health parses both shapes" {
    const gpa = testing.allocator;
    const ok = try std.json.parseFromSlice(Health, gpa,
        \\{"status":"ok","uptime":12345}
    , .{ .ignore_unknown_fields = true });
    defer ok.deinit();
    try testing.expectEqualStrings("ok", ok.value.status);
    try testing.expectEqual(@as(i64, 12345), ok.value.uptime.?);
    const down = try std.json.parseFromSlice(Health, gpa,
        \\{"status":"unavailable"}
    , .{ .ignore_unknown_fields = true });
    defer down.deinit();
    try testing.expect(down.value.uptime == null);
}

test "fallbackUsername is deterministic, wire-legal and address-bound" {
    const key_a = try secp.PrivateKey.fromHex("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    const key_b = try secp.PrivateKey.fromHex("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    const name_a1 = fallbackUsername(key_a.addressHex());
    const name_a2 = fallbackUsername(key_a.addressHex());
    const name_b = fallbackUsername(key_b.addressHex());
    try testing.expectEqualStrings(&name_a1, &name_a2);
    try testing.expect(!std.mem.eql(u8, &name_a1, &name_b));
    try testing.expect(name_a1.len >= 3 and name_a1.len <= 100);
    for (name_a1) |c| try testing.expect(std.ascii.isAlphanumeric(c));
}

test "room id validation mirrors the wire format" {
    try testing.expect(isValidRoomId("room_1749652739650_304e0eaf"));
    try testing.expect(isValidRoomId("static.note.0xabc-def_1"));
    try testing.expect(!isValidRoomId("short"));
    try testing.expect(!isValidRoomId("room_with/slash_123"));
    try testing.expect(!isValidRoomId("room id with spaces"));
    try testing.expect(!isValidRoomId("../../../etc/passwd"));
    try testing.expect(!isValidRoomId("room_x?limit=1&y=2"));
}
