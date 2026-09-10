//! Integration tests: every test boots a real `pocketskynet` process via
//! tests/harness.zig, drives it over the wire, and tears it down.
//!
//! Run with `zig build itest`. The server binary is found under
//! `app/target/{release,debug}/pocketskynet` (walking up from the cwd) or
//! `POCKETSKYNET_SERVER_BIN`; if absent it is built once with cargo.

const std = @import("std");
const ps = @import("pocketskynet");
const harness = @import("harness.zig");
const build_options = @import("itest_options");

const gpa = std.testing.allocator;

fn io() std.Io {
    return std.testing.io;
}

// Distinct, valid private keys for test personae.
const key_alice_hex = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const key_bob_hex = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const key_carol_hex = "0x00000000000000000000000000000000000000000000000000000000000000c3";

fn plainTransport() ps.transport.Transport {
    return ps.transport.Transport.init(gpa, io(), .{});
}

fn expectApiFailure(client: *ps.api.Client, result: anytype, status: u16) !void {
    if (result) |parsed| {
        parsed.deinit();
        return error.TestExpectedApiFailure;
    } else |err| {
        try std.testing.expectEqual(ps.api.Error.Api, err);
        try std.testing.expectEqual(status, client.last_status);
    }
}

fn errorMessageContains(client: *ps.api.Client, needle: []const u8) !void {
    const body = client.last_body orelse return error.TestNoErrorBody;
    const parsed = ps.api.parseErrorEnvelope(gpa, body) orelse return error.TestBadEnvelope;
    defer parsed.deinit();
    const msg = parsed.value.message orelse return error.TestNoMessage;
    if (std.mem.find(u8, msg, needle) == null) {
        std.debug.print("expected '{s}' in '{s}'\n", .{ needle, msg });
        return error.TestWrongMessage;
    }
}

// ---------------------------------------------------------------------------

test "login happy path: named first login, JWT works, built-in rooms exist" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    const login = try client.login(key, "alice");
    defer login.deinit();

    try std.testing.expect(login.value.token.len > 20);
    const addr = key.addressHex();
    try std.testing.expectEqualStrings(&addr, login.value.user.walletAddress);
    try std.testing.expectEqualStrings("alice", login.value.user.username);
    // encryptionSalt is served to the owner on login.
    try std.testing.expectEqual(@as(usize, 64), login.value.encryptionSalt.?.len);

    // The JWT works: the profile endpoint answers with the caller.
    const profile = try client.profile();
    defer profile.deinit();
    try std.testing.expectEqualStrings("alice", profile.value.username);

    // Fresh accounts hold the three built-in rooms. Assert membership by
    // name, never counts.
    const rooms = try client.listRooms();
    defer rooms.deinit();
    var have_note = false;
    var have_jarvis = false;
    var have_lobby = false;
    for (rooms.value) |room| {
        if (std.mem.eql(u8, room.name, "My Note")) have_note = true;
        if (std.mem.eql(u8, room.name, "My Jarvis")) have_jarvis = true;
        if (std.mem.eql(u8, room.name, "My Lobby")) have_lobby = true;
    }
    try std.testing.expect(have_note);
    try std.testing.expect(have_jarvis);
    try std.testing.expect(have_lobby);
}

test "login: first-time login without a username retries once with a generated one" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_bob_hex);
    // No username: the server burns the first challenge with
    // "Username is required for first-time login"; the client must fetch a
    // fresh challenge and retry with a generated name.
    const login = try client.login(key, null);
    defer login.deinit();
    const expected = ps.api.fallbackUsername(key.addressHex());
    try std.testing.expectEqualStrings(&expected, login.value.user.username);

    // Second login with no username reuses the stored one (no retry needed).
    var t2 = plainTransport();
    defer t2.deinit();
    var client2 = ps.api.Client.init(gpa, &t2, server.base_url);
    defer client2.deinit();
    const relogin = try client2.login(key, null);
    defer relogin.deinit();
    try std.testing.expectEqualStrings(&expected, relogin.value.user.username);
}

test "login: a wrong signature is 401 and the challenge is burned by the failure" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    const addr = key.addressHex();

    const challenge = try client.challenge(&addr);
    defer challenge.deinit();

    // Sign the WRONG message.
    const bad_sig = ps.eip191.personalSignHex("not the challenge", key);
    const bad_body = try ps.api.buildLoginBody(gpa, &addr, challenge.value.challengeId, &bad_sig, "alice");
    defer gpa.free(bad_body);
    const login_url = try std.fmt.allocPrint(gpa, "{s}/api/auth/login", .{server.base_url});
    defer gpa.free(login_url);

    var resp = try t.request(.POST, login_url, bad_body, null);
    defer resp.deinit(gpa);
    try std.testing.expectEqual(@as(u16, 401), resp.status);

    // The failed attempt burned the challenge: the CORRECT signature over the
    // same challenge is now refused as expired/invalid (400), not accepted.
    const good_sig = ps.eip191.personalSignHex(challenge.value.message, key);
    const good_body = try ps.api.buildLoginBody(gpa, &addr, challenge.value.challengeId, &good_sig, "alice");
    defer gpa.free(good_body);
    var replay = try t.request(.POST, login_url, good_body, null);
    defer replay.deinit(gpa);
    try std.testing.expectEqual(@as(u16, 400), replay.status);
}

test "login: a successful challenge cannot be replayed" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    const addr = key.addressHex();

    const challenge = try client.challenge(&addr);
    defer challenge.deinit();
    const sig = ps.eip191.personalSignHex(challenge.value.message, key);
    const body = try ps.api.buildLoginBody(gpa, &addr, challenge.value.challengeId, &sig, "alice");
    defer gpa.free(body);
    const login_url = try std.fmt.allocPrint(gpa, "{s}/api/auth/login", .{server.base_url});
    defer gpa.free(login_url);

    var first = try t.request(.POST, login_url, body, null);
    defer first.deinit(gpa);
    try std.testing.expectEqual(@as(u16, 200), first.status);

    // Same challengeId, same valid signature: burned by success.
    var replay = try t.request(.POST, login_url, body, null);
    defer replay.deinit(gpa);
    try std.testing.expectEqual(@as(u16, 400), replay.status);
}

test "auth: tampered and absent JWTs are 401" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    const login = try client.login(key, "alice");
    defer login.deinit();

    const rooms_url = try std.fmt.allocPrint(gpa, "{s}/api/rooms", .{server.base_url});
    defer gpa.free(rooms_url);

    // Sanity: the untampered token works.
    {
        var ok = try t.request(.GET, rooms_url, null, login.value.token);
        defer ok.deinit(gpa);
        try std.testing.expectEqual(@as(u16, 200), ok.status);
    }

    // Flip one character of the signature segment.
    const tampered = try gpa.dupe(u8, login.value.token);
    defer gpa.free(tampered);
    const last = tampered[tampered.len - 1];
    tampered[tampered.len - 1] = if (last == 'A') 'B' else 'A';
    {
        var resp = try t.request(.GET, rooms_url, null, tampered);
        defer resp.deinit(gpa);
        try std.testing.expectEqual(@as(u16, 401), resp.status);
    }

    // Garbage token.
    {
        var resp = try t.request(.GET, rooms_url, null, "not.a.jwt");
        defer resp.deinit(gpa);
        try std.testing.expectEqual(@as(u16, 401), resp.status);
    }

    // No Authorization header at all.
    {
        var resp = try t.request(.GET, rooms_url, null, null);
        defer resp.deinit(gpa);
        try std.testing.expectEqual(@as(u16, 401), resp.status);
    }
}

test "rooms: create, list, and reject an invalid name with the validation envelope" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    (try client.login(key, "alice")).deinit();

    const created = try client.createRoom("Zig Test Room", "made by the zig client");
    defer created.deinit();
    try std.testing.expect(ps.api.isValidRoomId(created.value.id));
    try std.testing.expectEqualStrings("Zig Test Room", created.value.name);

    // Listed among the caller's rooms.
    const rooms = try client.listRooms();
    defer rooms.deinit();
    var found = false;
    for (rooms.value) |room| {
        if (std.mem.eql(u8, room.id, created.value.id)) found = true;
    }
    try std.testing.expect(found);

    // Forbidden characters produce the `Validation failed` + errors[] shape.
    try expectApiFailure(&client, client.createRoom("bad<name>", null), 400);
    {
        const body = client.last_body.?;
        const env = ps.api.parseErrorEnvelope(gpa, body).?;
        defer env.deinit();
        try std.testing.expect(env.value.errors != null);
        try std.testing.expect(env.value.errors.?.len > 0);
    }
}

test "messages: send, list ascending, limit, unicode round trip" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    (try client.login(key, "alice")).deinit();
    const room = try client.createRoom("Chatter", null);
    defer room.deinit();
    const room_id = room.value.id;

    const texts = [_][]const u8{
        "first message",
        "second message",
        "한글 메시지 🍓🍊",
        "  padded needs trimming  ",
        "fifth",
    };
    for (texts) |text| {
        const sent = try client.sendText(room_id, text);
        defer sent.deinit();
        try std.testing.expectEqualStrings(&ps.msghash.msgHashPlaintext(text), sent.value.msgHash);
        // The server stores trimmed content.
        try std.testing.expectEqualStrings(ps.msghash.trim(text), sent.value.content);
    }

    // Full listing: chronologically ascending by (messageTimestamp, msgSerial),
    // contents in send order, sender attached.
    const listed = try client.listMessages(room_id, null);
    defer listed.deinit();
    try std.testing.expectEqual(texts.len, listed.value.len);
    var prev_ts: i64 = -1;
    var prev_serial: i64 = -1;
    for (listed.value, 0..) |msg, i| {
        try std.testing.expectEqualStrings(ps.msghash.trim(texts[i]), msg.content);
        try std.testing.expectEqualStrings("alice", msg.sender.?.username);
        if (msg.messageTimestamp == prev_ts) {
            try std.testing.expect(msg.msgSerial > prev_serial);
        } else {
            try std.testing.expect(msg.messageTimestamp > prev_ts);
        }
        prev_ts = msg.messageTimestamp;
        prev_serial = msg.msgSerial;
    }

    // limit=2 returns the two newest, still ascending.
    const limited = try client.listMessages(room_id, 2);
    defer limited.deinit();
    try std.testing.expectEqual(@as(usize, 2), limited.value.len);
    try std.testing.expectEqualStrings("padded needs trimming", limited.value[0].content);
    try std.testing.expectEqualStrings("fifth", limited.value[1].content);
}

test "messages: foreign and nonexistent rooms are both 403" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t_alice = plainTransport();
    defer t_alice.deinit();
    var alice = ps.api.Client.init(gpa, &t_alice, server.base_url);
    defer alice.deinit();
    const key_a = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    (try alice.login(key_a, "alice")).deinit();
    const room = try alice.createRoom("Private", null);
    defer room.deinit();

    var t_bob = plainTransport();
    defer t_bob.deinit();
    var bob = ps.api.Client.init(gpa, &t_bob, server.base_url);
    defer bob.deinit();
    const key_b = try ps.secp.PrivateKey.fromHex(key_bob_hex);
    (try bob.login(key_b, "bob")).deinit();

    // A room bob is not a member of: 403 Access denied.
    try expectApiFailure(&bob, bob.sendText(room.value.id, "let me in"), 403);
    try errorMessageContains(&bob, "Access denied");

    // A room that does not exist at all: the same 403, deliberately
    // indistinguishable (no room-id oracle).
    try expectApiFailure(&bob, bob.sendText("room_does_not_exist_0000", "hello"), 403);
    try errorMessageContains(&bob, "Access denied");

    // Listing is refused the same way.
    try expectApiFailure(&bob, bob.listMessages(room.value.id, null), 403);
}

test "messages: msgHash validation (missing, uppercase, wrong length)" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    (try client.login(key, "alice")).deinit();
    const room = try client.createRoom("Hashes", null);
    defer room.deinit();

    // Uppercase hex: refused (message hashes are lowercase-only).
    try expectApiFailure(&client, client.sendRaw(room.value.id, "hi", "AB" ** 32), 400);
    // Wrong length.
    try expectApiFailure(&client, client.sendRaw(room.value.id, "hi", "abcd"), 400);

    // Missing msgHash entirely (raw body).
    const url = try std.fmt.allocPrint(gpa, "{s}/api/rooms/{s}/messages", .{ server.base_url, room.value.id });
    defer gpa.free(url);
    var resp = try t.request(.POST, url, "{\"content\":\"hi\"}", client.token.?);
    defer resp.deinit(gpa);
    try std.testing.expectEqual(@as(u16, 400), resp.status);

    // And a correct hash still works after all those refusals.
    const ok = try client.sendText(room.value.id, "hi");
    ok.deinit();
}

test "messages: the 100KB body cap answers 413" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    var t = plainTransport();
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    (try client.login(key, "alice")).deinit();
    const room = try client.createRoom("Big", null);
    defer room.deinit();

    // > 100 KB of JSON body.
    const big = try gpa.alloc(u8, 110 * 1024);
    defer gpa.free(big);
    @memset(big, 'a');
    const result = client.sendRaw(room.value.id, big, "ab" ** 32);
    try expectApiFailure(&client, result, 413);
}

test "https: --insecure (curl -k) and CA-pinned std TLS both reach the server" {
    var server = try harness.TestServer.start(gpa, io(), .{ .tls = true });
    defer server.stop();

    // Path 1: --insecure routes through curl -k.
    var t_insecure = ps.transport.Transport.init(gpa, io(), .{ .insecure = true });
    defer t_insecure.deinit();
    var client = ps.api.Client.init(gpa, &t_insecure, server.base_url);
    defer client.deinit();

    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    (try client.login(key, "alice")).deinit();
    const room = try client.createRoom("Secure Room", null);
    defer room.deinit();
    const sent = try client.sendText(room.value.id, "over TLS");
    defer sent.deinit();
    try std.testing.expectEqualStrings("over TLS", sent.value.content);

    // Path 2: trust the server's generated CA through std.http's TLS stack
    // (no curl involved). std's verifier matches DNS SANs, not IP SANs, so
    // this path connects via `localhost` — a DNS name the certificate holds.
    const ca = try server.caPath(gpa);
    defer gpa.free(ca);
    const localhost_url = try std.fmt.allocPrint(gpa, "https://localhost:{d}", .{server.port});
    defer gpa.free(localhost_url);
    var t_pinned = ps.transport.Transport.init(gpa, io(), .{ .cacert = ca });
    defer t_pinned.deinit();
    var pinned = ps.api.Client.init(gpa, &t_pinned, localhost_url);
    defer pinned.deinit();
    const health = try pinned.health();
    defer health.deinit();
    try std.testing.expectEqualStrings("ok", health.value.status);
}

test "http3: full round trip over QUIC (or SKIP without an HTTP/3 curl)" {
    // Fail-fast probe: without an HTTP/3-capable curl this group is a SKIP.
    _ = ps.transport.findCurl(gpa, io(), null, true) catch {
        std.debug.print("SKIP: no HTTP/3-capable curl found (brew install curl provides one)\n", .{});
        return error.SkipZigTest;
    };

    var server = try harness.TestServer.start(gpa, io(), .{ .http3 = true });
    defer server.stop();

    const h3_url = try server.http3Url(gpa);
    defer gpa.free(h3_url);
    const ca = try server.caPath(gpa);
    defer gpa.free(ca);

    // --http3-only against the QUIC listener, trusting the generated CA.
    var t = ps.transport.Transport.init(gpa, io(), .{ .http3 = true, .cacert = ca });
    defer t.deinit();
    var client = ps.api.Client.init(gpa, &t, h3_url);
    defer client.deinit();

    const health = try client.health();
    defer health.deinit();
    try std.testing.expectEqualStrings("ok", health.value.status);

    const key = try ps.secp.PrivateKey.fromHex(key_carol_hex);
    const login = try client.login(key, "carol");
    defer login.deinit();
    const room = try client.createRoom("QUIC Room", null);
    defer room.deinit();
    const sent = try client.sendText(room.value.id, "hello over HTTP/3 🍓");
    defer sent.deinit();
    const listed = try client.listMessages(room.value.id, null);
    defer listed.deinit();
    try std.testing.expectEqual(@as(usize, 1), listed.value.len);
    try std.testing.expectEqualStrings("hello over HTTP/3 🍓", listed.value[0].content);

    // The TCP listener answers the same API over HTTP/1.1 beside it.
    var t_tcp = plainTransport();
    defer t_tcp.deinit();
    var tcp_client = ps.api.Client.init(gpa, &t_tcp, server.base_url);
    defer tcp_client.deinit();
    const tcp_health = try tcp_client.health();
    defer tcp_health.deinit();
    try std.testing.expectEqualStrings("ok", tcp_health.value.status);
}

// -- CLI --------------------------------------------------------------------

fn runCli(argv: []const []const u8) !std.process.RunResult {
    return std.process.run(gpa, io(), .{
        .argv = argv,
        .stdout_limit = .limited(1 << 20),
        .stderr_limit = .limited(1 << 20),
    });
}

fn exitCode(term: std.process.Child.Term) !u8 {
    return switch (term) {
        .exited => |code| code,
        else => error.TestCliDidNotExit,
    };
}

test "cli: exit codes and a full command round trip" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    const exe = build_options.cli_exe;

    // health → 0
    {
        const r = try runCli(&.{ exe, "health", "--server", server.base_url });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 0), try exitCode(r.term));
        try std.testing.expect(std.mem.find(u8, r.stdout, "status: ok") != null);
    }

    // unreachable server → 1
    {
        const r = try runCli(&.{ exe, "health", "--server", "http://127.0.0.1:1" });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 1), try exitCode(r.term));
    }

    // unknown command → 2
    {
        const r = try runCli(&.{ exe, "bogus-command", "--server", server.base_url });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 2), try exitCode(r.term));
    }

    // auth command without a key → 2
    {
        const r = try runCli(&.{ exe, "rooms", "--server", server.base_url });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 2), try exitCode(r.term));
    }

    // login → create-room → send → messages, all through the binary.
    {
        const r = try runCli(&.{ exe, "login", "--server", server.base_url, "--key", key_alice_hex, "--username", "cliuser" });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 0), try exitCode(r.term));
        try std.testing.expect(std.mem.find(u8, r.stdout, "username: cliuser") != null);
        try std.testing.expect(std.mem.find(u8, r.stdout, "token:") != null);
    }

    var room_id_buf: [128]u8 = undefined;
    var room_id: []const u8 = "";
    {
        const r = try runCli(&.{ exe, "create-room", "CLI Room", "--server", server.base_url, "--key", key_alice_hex });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 0), try exitCode(r.term));
        const tab = std.mem.findScalar(u8, r.stdout, '\t') orelse return error.TestNoRoomId;
        @memcpy(room_id_buf[0..tab], r.stdout[0..tab]);
        room_id = room_id_buf[0..tab];
    }
    {
        const r = try runCli(&.{ exe, "send", room_id, "hello from the CLI", "--server", server.base_url, "--key", key_alice_hex });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 0), try exitCode(r.term));
    }
    {
        const r = try runCli(&.{ exe, "messages", room_id, "--server", server.base_url, "--key", key_alice_hex });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 0), try exitCode(r.term));
        try std.testing.expect(std.mem.find(u8, r.stdout, "hello from the CLI") != null);
    }

    // sending to a foreign/nonexistent room → 1 (API failure, not usage).
    {
        const r = try runCli(&.{ exe, "send", "room_not_yours_000", "x", "--server", server.base_url, "--key", key_bob_hex });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try std.testing.expectEqual(@as(u8, 1), try exitCode(r.term));
        try std.testing.expect(std.mem.find(u8, r.stderr, "Access denied") != null);
    }
}

// -- concurrency ------------------------------------------------------------

const SendWorker = struct {
    base_url: []const u8,
    room_id: []const u8,
    key_hex: []const u8,
    index: usize,
    serials: []i64, // slots [index*sends_per_worker ..][0..sends_per_worker]
    ok: bool = false,

    const sends_per_worker = 5;

    fn run(self: *SendWorker) void {
        self.runInner() catch |err| {
            std.debug.print("worker {d} failed: {t}\n", .{ self.index, err });
            self.ok = false;
            return;
        };
        self.ok = true;
    }

    fn runInner(self: *SendWorker) !void {
        var worker_gpa = std.heap.page_allocator;
        var t = ps.transport.Transport.init(worker_gpa, io(), .{});
        defer t.deinit();
        var client = ps.api.Client.init(worker_gpa, &t, self.base_url);
        defer client.deinit();
        const key = try ps.secp.PrivateKey.fromHex(self.key_hex);
        (try client.login(key, null)).deinit();

        var buf: [64]u8 = undefined;
        for (0..sends_per_worker) |i| {
            const text = try std.fmt.bufPrint(&buf, "worker {d} message {d}", .{ self.index, i });
            const sent = try client.sendText(self.room_id, text);
            defer sent.deinit();
            self.serials[self.index * sends_per_worker + i] = sent.value.msgSerial;
        }
        _ = &worker_gpa;
    }
};

test "concurrency: parallel sends land with distinct msgSerials" {
    var server = try harness.TestServer.start(gpa, io(), .{});
    defer server.stop();

    // One member, one room; four threads share the wallet, each with its own
    // transport, client and login.
    var setup_t = plainTransport();
    defer setup_t.deinit();
    var setup = ps.api.Client.init(gpa, &setup_t, server.base_url);
    defer setup.deinit();
    const key = try ps.secp.PrivateKey.fromHex(key_alice_hex);
    (try setup.login(key, "alice")).deinit();
    const room = try setup.createRoom("Busy Room", null);
    defer room.deinit();

    const worker_count = 4;
    const total = worker_count * SendWorker.sends_per_worker;
    var serials = [_]i64{-1} ** total;

    var workers: [worker_count]SendWorker = undefined;
    for (&workers, 0..) |*w, i| {
        w.* = .{
            .base_url = server.base_url,
            .room_id = room.value.id,
            .key_hex = key_alice_hex,
            .index = i,
            .serials = &serials,
        };
    }

    var threads: [worker_count]std.Thread = undefined;
    for (&threads, 0..) |*thread, i| {
        thread.* = try std.Thread.spawn(.{}, SendWorker.run, .{&workers[i]});
    }
    for (&threads) |*thread| thread.join();
    for (&workers) |*w| try std.testing.expect(w.ok);

    // Every send got a serial, and all serials are distinct.
    for (serials) |serial| try std.testing.expect(serial > 0);
    for (serials, 0..) |a, i| {
        for (serials[i + 1 ..]) |b| {
            try std.testing.expect(a != b);
        }
    }

    // And the server agrees: the room holds exactly `total` messages with
    // those serials.
    const listed = try setup.listMessages(room.value.id, 100);
    defer listed.deinit();
    try std.testing.expectEqual(@as(usize, total), listed.value.len);
}
