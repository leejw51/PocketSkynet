//! `pskynet-zig` — CLI for the PocketSkynet server.
//!
//! Commands: login, rooms, create-room, send, messages, health.
//! Exit codes: 0 success, 1 runtime/API failure, 2 usage error.

const std = @import("std");
const ps = @import("pocketskynet");

const usage =
    \\pskynet-zig — PocketSkynet client (Zig)
    \\
    \\Usage: pskynet-zig [flags] <command> [args]
    \\
    \\Commands:
    \\  health                     server health check (no auth)
    \\  login                      log in, print the JWT
    \\  rooms                      list rooms (logs in first)
    \\  create-room <name>         create a room
    \\  send <roomId> <text>       send a plaintext message
    \\  messages <roomId>          list messages in a room
    \\
    \\Flags:
    \\  --server <url>       base URL (default http://127.0.0.1:9099)
    \\  --key <hex>          wallet private key (or POCKETSKYNET_KEY env)
    \\  --username <name>    username for first-time login
    \\  --token <jwt>        skip login, use this JWT (or POCKETSKYNET_TOKEN)
    \\  --http3              use HTTP/3 (QUIC; needs an HTTP/3-capable curl)
    \\  --insecure           accept self-signed TLS certificates (via curl -k)
    \\  --cacert <pem>       trust this CA for HTTPS (std TLS path)
    \\  --limit <n>          messages: page size (1-100)
    \\
;

const Cli = struct {
    server: []const u8 = "http://127.0.0.1:9099",
    key: ?[]const u8 = null,
    username: ?[]const u8 = null,
    token: ?[]const u8 = null,
    http3: bool = false,
    insecure: bool = false,
    cacert: ?[]const u8 = null,
    limit: ?u32 = null,
    command: []const u8 = "",
    args: [2]?[]const u8 = .{ null, null },
};

fn fail(comptime fmt: []const u8, args: anytype) u8 {
    std.debug.print(fmt ++ "\n", .{} ++ args);
    return 1;
}

fn usageError(comptime fmt: []const u8, args: anytype) u8 {
    std.debug.print(fmt ++ "\n\n{s}", .{} ++ args ++ .{usage});
    return 2;
}

pub fn main(init: std.process.Init) !u8 {
    const gpa = init.gpa;
    const io = init.io;

    var argv_it = std.process.Args.Iterator.init(init.minimal.args);
    _ = argv_it.next(); // argv[0]

    var cli = Cli{};
    var positional: usize = 0;
    while (argv_it.next()) |arg| {
        if (std.mem.eql(u8, arg, "--server")) {
            cli.server = argv_it.next() orelse return usageError("--server needs a value", .{});
        } else if (std.mem.eql(u8, arg, "--key")) {
            cli.key = argv_it.next() orelse return usageError("--key needs a value", .{});
        } else if (std.mem.eql(u8, arg, "--username")) {
            cli.username = argv_it.next() orelse return usageError("--username needs a value", .{});
        } else if (std.mem.eql(u8, arg, "--token")) {
            cli.token = argv_it.next() orelse return usageError("--token needs a value", .{});
        } else if (std.mem.eql(u8, arg, "--cacert")) {
            cli.cacert = argv_it.next() orelse return usageError("--cacert needs a value", .{});
        } else if (std.mem.eql(u8, arg, "--limit")) {
            const raw = argv_it.next() orelse return usageError("--limit needs a value", .{});
            cli.limit = std.fmt.parseInt(u32, raw, 10) catch
                return usageError("--limit must be a number, got '{s}'", .{raw});
        } else if (std.mem.eql(u8, arg, "--http3")) {
            cli.http3 = true;
        } else if (std.mem.eql(u8, arg, "--insecure")) {
            cli.insecure = true;
        } else if (std.mem.eql(u8, arg, "--help") or std.mem.eql(u8, arg, "-h")) {
            std.debug.print("{s}", .{usage});
            return 0;
        } else if (std.mem.startsWith(u8, arg, "--")) {
            return usageError("unknown flag '{s}'", .{arg});
        } else if (cli.command.len == 0) {
            cli.command = arg;
        } else if (positional < cli.args.len) {
            cli.args[positional] = arg;
            positional += 1;
        } else {
            return usageError("too many arguments (at '{s}')", .{arg});
        }
    }

    if (cli.command.len == 0) return usageError("no command given", .{});

    if (cli.key == null) cli.key = init.environ_map.get("POCKETSKYNET_KEY");
    if (cli.token == null) cli.token = init.environ_map.get("POCKETSKYNET_TOKEN");

    if (cli.http3 and !std.mem.startsWith(u8, cli.server, "https://")) {
        return usageError("--http3 needs an https:// server URL (QUIC mandates TLS)", .{});
    }

    var t = ps.transport.Transport.init(gpa, io, .{
        .http3 = cli.http3,
        .insecure = cli.insecure,
        .cacert = cli.cacert,
    });
    defer t.deinit();

    var client = ps.api.Client.init(gpa, &t, cli.server);
    defer client.deinit();

    var stdout_buffer: [8192]u8 = undefined;
    var stdout_writer = std.Io.File.stdout().writer(io, &stdout_buffer);
    const out = &stdout_writer.interface;

    const code = run(&cli, &client, out) catch |err| blk: {
        break :blk reportError(gpa, err, &client, &t);
    };
    out.flush() catch {};
    return code;
}

fn reportError(gpa: std.mem.Allocator, err: anyerror, client: *ps.api.Client, t: *ps.transport.Transport) u8 {
    switch (err) {
        error.Api => {
            const text = client.lastFailureText(gpa) catch return fail("API error", .{});
            defer gpa.free(text);
            return fail("error: {s}", .{text});
        },
        error.NoHttp3Curl => return fail(
            "error: --http3 needs an HTTP/3-capable curl (Features: HTTP3).\n" ++
                "None found via POCKETSKYNET_CURL, /opt/homebrew/opt/curl/bin/curl,\n" ++
                "/usr/local/opt/curl/bin/curl or PATH. `brew install curl` provides one.",
            .{},
        ),
        error.CurlFailed => {
            if (t.last_curl_error) |msg| return fail("error: curl failed: {s}", .{msg});
            return fail("error: curl failed", .{});
        },
        error.HttpFailed => return fail("error: could not reach the server", .{}),
        error.InvalidPrivateKey => return fail("error: invalid private key (need 32 bytes of hex, 0 < key < n)", .{}),
        error.InvalidId => return fail("error: invalid room id", .{}),
        error.MissingKey => return usageError("this command needs --key <hex> or POCKETSKYNET_KEY", .{}),
        error.MissingArgument => return usageError("missing argument", .{}),
        else => return fail("error: {s}", .{@errorName(err)}),
    }
}

fn ensureAuth(cli: *const Cli, client: *ps.api.Client) !void {
    if (cli.token) |token| {
        try client.setToken(token);
        return;
    }
    const key_hex = cli.key orelse return error.MissingKey;
    const key = try ps.secp.PrivateKey.fromHex(key_hex);
    const login = try client.login(key, cli.username);
    login.deinit();
}

fn run(cli: *const Cli, client: *ps.api.Client, out: *std.Io.Writer) !u8 {
    if (std.mem.eql(u8, cli.command, "health")) {
        const parsed = try client.health();
        defer parsed.deinit();
        try out.print("status: {s}", .{parsed.value.status});
        if (parsed.value.uptime) |uptime| try out.print(" (uptime {d}s)", .{uptime});
        try out.print("\n", .{});
        return 0;
    }

    if (std.mem.eql(u8, cli.command, "login")) {
        const key_hex = cli.key orelse return error.MissingKey;
        const key = try ps.secp.PrivateKey.fromHex(key_hex);
        const login = try client.login(key, cli.username);
        defer login.deinit();
        try out.print("address:  {s}\n", .{login.value.user.walletAddress});
        try out.print("username: {s}\n", .{login.value.user.username});
        try out.print("token:    {s}\n", .{login.value.token});
        return 0;
    }

    if (std.mem.eql(u8, cli.command, "rooms")) {
        try ensureAuth(cli, client);
        const rooms = try client.listRooms();
        defer rooms.deinit();
        for (rooms.value) |room| {
            try out.print("{s}\t{s}", .{ room.id, room.name });
            if (room.unreadCount) |unread| {
                if (unread > 0) try out.print("\t({d} unread)", .{unread});
            }
            try out.print("\n", .{});
        }
        return 0;
    }

    if (std.mem.eql(u8, cli.command, "create-room")) {
        const name = cli.args[0] orelse return error.MissingArgument;
        try ensureAuth(cli, client);
        const room = try client.createRoom(name, null);
        defer room.deinit();
        try out.print("{s}\t{s}\n", .{ room.value.id, room.value.name });
        return 0;
    }

    if (std.mem.eql(u8, cli.command, "send")) {
        const room_id = cli.args[0] orelse return error.MissingArgument;
        const text = cli.args[1] orelse return error.MissingArgument;
        try ensureAuth(cli, client);
        const msg = try client.sendText(room_id, text);
        defer msg.deinit();
        try out.print("{s}\tserial {d}\n", .{ msg.value.id, msg.value.msgSerial });
        return 0;
    }

    if (std.mem.eql(u8, cli.command, "messages")) {
        const room_id = cli.args[0] orelse return error.MissingArgument;
        try ensureAuth(cli, client);
        const msgs = try client.listMessages(room_id, cli.limit);
        defer msgs.deinit();
        for (msgs.value) |msg| {
            const who = if (msg.sender) |sender| sender.username else msg.senderAddress;
            try out.print("[{d}] {s}: {s}\n", .{ msg.messageTimestamp, who, msg.content });
        }
        return 0;
    }

    std.debug.print("unknown command '{s}'\n\n{s}", .{ cli.command, usage });
    return 2;
}
