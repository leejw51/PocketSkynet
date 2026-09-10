//! Boots a real `pocketskynet` server per test, modeled on the Rust suite's
//! `app/server/tests/common/harness.rs`.
//!
//! Each server gets its own ephemeral port, its own temp data directory and a
//! guaranteed teardown (`defer server.stop()` in every test; the Zig test
//! runner runs tests sequentially, and a failed test unwinds through its
//! defers). Bind races are detected the same way the Rust harness does: after
//! `/api/health` answers, the child must still be alive — if it lost the bind
//! it has already exited and the 200 came from the winner.

const std = @import("std");
const ps = @import("pocketskynet");

/// Handed to the server with `--jwt-secret` so tests can mint tampered
/// tokens themselves.
pub const jwt_secret = "pocketskynet-zig-integration-test-secret-0123456789";

const boot_timeout_ms: i64 = 30_000;

var dir_counter: std.atomic.Value(u64) = .init(0);

// -- server binary discovery ------------------------------------------------

const bin_candidates = [_][]const u8{
    "app/target/release/pocketskynet",
    "app/target/debug/pocketskynet",
    // In case the checkout is the absorbed `app/` submodule itself.
    "target/release/pocketskynet",
    "target/debug/pocketskynet",
};

fn exists(io: std.Io, path: []const u8) bool {
    std.Io.Dir.cwd().access(io, path, .{}) catch return false;
    return true;
}

fn toAbsolute(gpa: std.mem.Allocator, io: std.Io, path: []const u8) ![]u8 {
    if (std.fs.path.isAbsolute(path)) return gpa.dupe(u8, path);
    const cwd = try std.process.currentPathAlloc(io, gpa);
    defer gpa.free(cwd);
    return std.fs.path.resolve(gpa, &.{ cwd, path });
}

/// Locate the server binary: `POCKETSKYNET_SERVER_BIN`, then
/// `{ancestor}/app/target/{release,debug}/pocketskynet` walking up from the
/// cwd. If none exists but a Cargo workspace does, build it once.
pub fn findServerBinary(gpa: std.mem.Allocator, io: std.Io) ![]u8 {
    if (ps.transport.getEnv("POCKETSKYNET_SERVER_BIN")) |bin| {
        if (exists(io, bin)) return toAbsolute(gpa, io, bin);
        std.debug.print("POCKETSKYNET_SERVER_BIN={s} does not exist\n", .{bin});
        return error.ServerBinaryNotFound;
    }

    var prefix: std.ArrayList(u8) = .empty;
    defer prefix.deinit(gpa);
    var depth: usize = 0;
    while (depth < 12) : (depth += 1) {
        for (bin_candidates) |candidate| {
            const path = try std.fmt.allocPrint(gpa, "{s}{s}", .{ prefix.items, candidate });
            defer gpa.free(path);
            if (exists(io, path)) return toAbsolute(gpa, io, path);
        }
        try prefix.appendSlice(gpa, "../");
    }

    // Not built yet: find the Cargo workspace and build it once.
    prefix.clearRetainingCapacity();
    depth = 0;
    while (depth < 12) : (depth += 1) {
        const manifest = try std.fmt.allocPrint(gpa, "{s}app/Cargo.toml", .{prefix.items});
        defer gpa.free(manifest);
        if (exists(io, manifest)) {
            const app_dir = try std.fmt.allocPrint(gpa, "{s}app", .{prefix.items});
            defer gpa.free(app_dir);
            std.debug.print("building pocketskynet (cargo build --release) in {s}...\n", .{app_dir});
            const result = std.process.run(gpa, io, .{
                .argv = &.{ "cargo", "build", "--release", "--bin", "pocketskynet" },
                .cwd = .{ .path = app_dir },
                .stdout_limit = .limited(1 << 20),
                .stderr_limit = .limited(4 << 20),
            }) catch |err| {
                std.debug.print("could not run cargo: {t}\n", .{err});
                return error.ServerBinaryNotFound;
            };
            defer gpa.free(result.stdout);
            defer gpa.free(result.stderr);
            switch (result.term) {
                .exited => |code| if (code != 0) {
                    std.debug.print("cargo build failed:\n{s}\n", .{result.stderr});
                    return error.ServerBinaryNotFound;
                },
                else => return error.ServerBinaryNotFound,
            }
            const built = try std.fmt.allocPrint(gpa, "{s}app/target/release/pocketskynet", .{prefix.items});
            defer gpa.free(built);
            if (exists(io, built)) return toAbsolute(gpa, io, built);
            return error.ServerBinaryNotFound;
        }
        try prefix.appendSlice(gpa, "../");
    }

    std.debug.print(
        "pocketskynet server binary not found. Build it with `cargo build --release` " ++
            "in app/, or set POCKETSKYNET_SERVER_BIN.\n",
        .{},
    );
    return error.ServerBinaryNotFound;
}

// -- ports ------------------------------------------------------------------

pub fn freeTcpPort(io: std.Io) !u16 {
    const addr = try std.Io.net.IpAddress.parse("127.0.0.1", 0);
    var server = try addr.listen(io, .{});
    defer server.deinit(io);
    return switch (server.socket.address) {
        .ip4 => |a| a.port,
        .ip6 => |a| a.port,
    };
}

pub fn freeUdpPort(io: std.Io) !u16 {
    const addr = try std.Io.net.IpAddress.parse("127.0.0.1", 0);
    var socket = try addr.bind(io, .{ .mode = .dgram });
    defer socket.close(io);
    return switch (socket.address) {
        .ip4 => |a| a.port,
        .ip6 => |a| a.port,
    };
}

// -- the server -------------------------------------------------------------

pub const Options = struct {
    tls: bool = false,
    http3: bool = false,
    extra_args: []const []const u8 = &.{},
};

pub const TestServer = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    child: std.process.Child,
    reaped: bool = false,
    port: u16,
    http3_port: ?u16 = null,
    redirect_port: ?u16 = null,
    tls: bool,
    data_dir: []u8,
    base_url: []u8,
    bin_path: []u8,

    pub fn start(gpa: std.mem.Allocator, io: std.Io, opts: Options) !TestServer {
        var last_err: anyerror = error.BootFailed;
        for (0..5) |_| {
            return tryStart(gpa, io, opts) catch |err| {
                // A missing binary will never appear on a retry, and each
                // attempt could trigger a full `cargo build` — fail fast.
                if (err == error.ServerBinaryNotFound) return err;
                last_err = err;
                continue;
            };
        }
        std.debug.print("could not start pocketskynet after 5 attempts: {t}\n", .{last_err});
        return last_err;
    }

    fn tryStart(gpa: std.mem.Allocator, io: std.Io, opts: Options) !TestServer {
        const bin_path = try findServerBinary(gpa, io);
        errdefer gpa.free(bin_path);

        const port = try freeTcpPort(io);
        const http3_port: ?u16 = if (opts.http3) try freeUdpPort(io) else null;
        const redirect_port: ?u16 = if (opts.tls) try freeTcpPort(io) else null;

        // Unique data dir under the system temp directory.
        const tmp_root = ps.transport.getEnv("TMPDIR") orelse "/tmp";
        const seq = dir_counter.fetchAdd(1, .monotonic);
        const now = std.Io.Clock.real.now(io).toNanoseconds();
        const data_dir = try std.fmt.allocPrint(gpa, "{s}{s}ps-zig-it-{d}-{d}-{d}", .{
            tmp_root,
            if (std.mem.endsWith(u8, tmp_root, "/")) "" else "/",
            std.c.getpid(),
            now,
            seq,
        });
        errdefer gpa.free(data_dir);

        const cwd = std.Io.Dir.cwd();
        try cwd.createDirPath(io, data_dir);
        errdefer cwd.deleteTree(io, data_dir) catch {};

        // An empty static dir of its own, like the Rust harness.
        const static_dir = try std.fmt.allocPrint(gpa, "{s}/static", .{data_dir});
        defer gpa.free(static_dir);
        try cwd.createDirPath(io, static_dir);

        // Logs go to a file: piped stdio would deadlock the child once the
        // pipe buffer filled.
        const log_path = try std.fmt.allocPrint(gpa, "{s}/server.log", .{data_dir});
        defer gpa.free(log_path);
        const log_file = try std.Io.Dir.createFileAbsolute(io, log_path, .{});
        defer log_file.close(io);

        // Numeric argv values need to outlive the spawn call; keep them in a
        // scratch list freed on exit from this function.
        var scratch: std.ArrayList([]u8) = .empty;
        defer {
            for (scratch.items) |s| gpa.free(s);
            scratch.deinit(gpa);
        }
        const port_s = try std.fmt.allocPrint(gpa, "{d}", .{port});
        try scratch.append(gpa, port_s);

        var argv: std.ArrayList([]const u8) = .empty;
        defer argv.deinit(gpa);
        try argv.appendSlice(gpa, &.{
            bin_path,
            "--host",
            "127.0.0.1",
            "--port",
            port_s,
            "--data-dir",
            data_dir,
            "--static-dir",
            static_dir,
            "--jwt-secret",
            jwt_secret,
            "--no-rate-limit",
            "--no-payment-verify",
            "--no-mdns",
            "--log",
            "warn",
        });
        if (opts.tls) {
            const redirect_s = try std.fmt.allocPrint(gpa, "{d}", .{redirect_port.?});
            try scratch.append(gpa, redirect_s);
            try argv.appendSlice(gpa, &.{ "--tls", "--http-redirect-port", redirect_s });
        }
        if (opts.http3) {
            const h3_s = try std.fmt.allocPrint(gpa, "{d}", .{http3_port.?});
            try scratch.append(gpa, h3_s);
            try argv.appendSlice(gpa, &.{ "--http3", "--http3-port", h3_s });
        }
        try argv.appendSlice(gpa, opts.extra_args);

        // Scrubbed environment: the server sees only what the harness sets,
        // never the developer's shell (PS_*, VITE_*), and the baked-in `make
        // build` values are disabled too.
        var env_map = std.process.Environ.Map.init(gpa);
        defer env_map.deinit();
        try env_map.put("PS_IGNORE_BAKED_ENV", "1");

        const child = try std.process.spawn(io, .{
            .argv = argv.items,
            .environ_map = &env_map,
            .stdin = .ignore,
            .stdout = .{ .file = log_file },
            .stderr = .{ .file = log_file },
        });

        const scheme: []const u8 = if (opts.tls) "https" else "http";
        const base_url = try std.fmt.allocPrint(gpa, "{s}://127.0.0.1:{d}", .{ scheme, port });
        errdefer gpa.free(base_url);

        var server = TestServer{
            .gpa = gpa,
            .io = io,
            .child = child,
            .port = port,
            .http3_port = http3_port,
            .redirect_port = redirect_port,
            .tls = opts.tls,
            .data_dir = data_dir,
            .base_url = base_url,
            .bin_path = bin_path,
        };

        server.awaitHealth() catch |err| {
            server.dumpLogTail();
            server.stop();
            return err;
        };
        return server;
    }

    /// True while the child has not exited. Reaps only on a *real* exit, so a
    /// transient `waitpid` error (e.g. EINTR, rc = -1) never makes us treat a
    /// still-running server as dead — which would leak it past teardown.
    fn childAlive(self: *TestServer) bool {
        if (self.reaped) return false;
        const pid = self.child.id orelse return false;
        var status: c_int = 0;
        const rc = std.c.waitpid(pid, &status, std.posix.W.NOHANG);
        if (rc == pid) {
            // The child was reaped by this call.
            self.reaped = true;
            return false;
        }
        // rc == 0: still running. rc < 0: waitpid error (e.g. EINTR); assume
        // still running and let a later call (or `stop`) reap it.
        return true;
    }

    fn awaitHealth(self: *TestServer) !void {
        // Over TLS the probe must accept the just-generated self-signed CA —
        // the curl `-k` path. Plain HTTP probes ride the std backend.
        var t = ps.transport.Transport.init(self.gpa, self.io, .{ .insecure = self.tls });
        defer t.deinit();

        const url = try std.fmt.allocPrint(self.gpa, "{s}/api/health", .{self.base_url});
        defer self.gpa.free(url);

        const start_ts = std.Io.Clock.real.now(self.io);
        while (true) {
            if (!self.childAlive()) return error.ServerExitedDuringBoot;
            if (t.request(.GET, url, null, null)) |resp| {
                var r = resp;
                defer r.deinit(self.gpa);
                if (r.status == 200) break;
            } else |_| {}
            const elapsed = start_ts.durationTo(std.Io.Clock.real.now(self.io)).toMilliseconds();
            if (elapsed > boot_timeout_ms) return error.HealthTimeout;
            std.Io.sleep(self.io, .fromMilliseconds(20), .awake) catch {};
        }

        // Bind-race detection: somebody answered — make sure it was us.
        if (!self.childAlive()) return error.LostBindRace;

        // The CA is written during startup; with TLS or HTTP/3 on, wait for
        // the file so clients have something to trust.
        if (self.tls or self.http3_port != null) {
            const ca = try self.caPath(self.gpa);
            defer self.gpa.free(ca);
            const deadline_start = std.Io.Clock.real.now(self.io);
            while (!existsAbs(self.io, ca)) {
                const waited = deadline_start.durationTo(std.Io.Clock.real.now(self.io)).toMilliseconds();
                if (waited > 10_000) return error.CaTimeout;
                std.Io.sleep(self.io, .fromMilliseconds(20), .awake) catch {};
            }
        }
    }

    fn dumpLogTail(self: *TestServer) void {
        const log_path = std.fmt.allocPrint(self.gpa, "{s}/server.log", .{self.data_dir}) catch return;
        defer self.gpa.free(log_path);
        const text = std.Io.Dir.cwd().readFileAlloc(self.io, log_path, self.gpa, .limited(1 << 20)) catch return;
        defer self.gpa.free(text);
        const tail = if (text.len > 2000) text[text.len - 2000 ..] else text;
        std.debug.print("--- server log (tail) ---\n{s}\n", .{tail});
    }

    /// Path of the CA the server generated (TLS and/or HTTP/3 servers).
    /// Caller frees.
    pub fn caPath(self: *const TestServer, gpa: std.mem.Allocator) ![]u8 {
        return std.fmt.allocPrint(gpa, "{s}/tls/ca.crt", .{self.data_dir});
    }

    /// Base URL of the HTTP/3 (QUIC) listener. Caller frees.
    pub fn http3Url(self: *const TestServer, gpa: std.mem.Allocator) ![]u8 {
        const port = self.http3_port orelse return error.NoHttp3Listener;
        return std.fmt.allocPrint(gpa, "https://127.0.0.1:{d}", .{port});
    }

    /// Kill the child, reap it, delete the data directory.
    pub fn stop(self: *TestServer) void {
        if (!self.reaped) {
            self.child.kill(self.io);
            self.reaped = true;
        }
        std.Io.Dir.cwd().deleteTree(self.io, self.data_dir) catch {};
        self.gpa.free(self.data_dir);
        self.gpa.free(self.base_url);
        self.gpa.free(self.bin_path);
        self.* = undefined;
    }
};

fn existsAbs(io: std.Io, path: []const u8) bool {
    std.Io.Dir.accessAbsolute(io, path, .{}) catch return false;
    return true;
}
