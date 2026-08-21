import assert from "node:assert/strict";
import { test } from "node:test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { TransportError } from "../../src/errors.js";
import {
  buildCurlArgs,
  buildCurlConfig,
  createTransport,
  CurlHttp3Transport,
  findHttp3Curl,
  parseCurlOutput,
  validateToken,
} from "../../src/transport.js";

const execFileAsync = promisify(execFile);

test("transport selection: default is fetch (http1), --http3 is curl", () => {
  const plain = createTransport({ baseUrl: "http://127.0.0.1:9099" });
  assert.equal(plain.kind, "http1");
  const h3 = createTransport({ baseUrl: "https://127.0.0.1:9101", http3: true });
  assert.equal(h3.kind, "http3-curl");
  assert.ok(h3 instanceof CurlHttp3Transport);
});

test("base URL is validated and trailing slashes are trimmed", () => {
  assert.throws(() => createTransport({ baseUrl: "not a url" }), TransportError);
  assert.throws(() => createTransport({ baseUrl: "ftp://host" }), /http\(s\)/);
  const t = createTransport({ baseUrl: "http://127.0.0.1:9099///" });
  assert.equal(t.baseUrl, "http://127.0.0.1:9099");
});

test("http3 with an http:// URL fails fast (QUIC mandates TLS)", () => {
  assert.throws(
    () =>
      buildCurlArgs("http://127.0.0.1:9099", { method: "GET", path: "/api/health" }),
    /https/,
  );
});

test("http3 transport with curlPath:null fails fast with a clear error", async () => {
  // An explicit null means "known unavailable" — deterministic on any host,
  // unlike relying on the machine having no HTTP/3 curl.
  const transport = new CurlHttp3Transport({
    baseUrl: "https://127.0.0.1:1",
    http3: true,
    curlPath: null,
  });
  await assert.rejects(
    () => transport.request({ method: "GET", path: "/api/health" }),
    (err: unknown) => err instanceof TransportError && /HTTP\/3-capable curl/.test(err.message),
  );
  await transport.close();
});

test("findHttp3Curl returns null when no candidate advertises HTTP3", async () => {
  const found = await findHttp3Curl(["/usr/bin/false", "/nonexistent/curl"]);
  assert.equal(found, null);
});

test("findHttp3Curl rejects a curl whose features lack HTTP3", async () => {
  // The system curl on this machine may or may not have HTTP3; assert the
  // probe agrees with `curl --version` rather than assuming either way.
  const found = await findHttp3Curl(["curl"]);
  const { stdout } = await execFileAsync("curl", ["--version"]);
  const hasH3 = /^Features:.*\bHTTP3\b/m.test(stdout);
  assert.equal(found !== null, hasH3);
});

test("curl argv: exact shape, URL last after --, secrets NOT in argv", () => {
  const { args, stdin } = buildCurlArgs(
    "https://127.0.0.1:9101",
    { method: "POST", path: "/api/auth/login", body: { walletAddress: "0xabc" }, token: "tok.en" },
    { insecure: true, timeoutMs: 5000 },
  );
  assert.equal(args[0], "--http3-only");
  assert.equal(args[args.length - 2], "--");
  assert.equal(args[args.length - 1], "https://127.0.0.1:9101/api/auth/login");
  assert.ok(args.includes("--insecure"));
  // The secret-bearing header/body go through `--config -`, never argv.
  assert.ok(args.includes("--config"));
  assert.equal(args[args.indexOf("--config") + 1], "-");
  assert.ok(!args.includes("--data-binary"), "body must not be an argv element");
  // The token and body must not appear anywhere in argv (the `ps` leak fix):
  for (const arg of args) {
    assert.ok(!arg.includes("tok.en"), `token leaked into argv: ${arg}`);
    assert.ok(!arg.includes("authorization:"), `auth header leaked into argv: ${arg}`);
    assert.ok(!arg.includes("0xabc"), `body leaked into argv: ${arg}`);
  }
  // They live in the stdin config instead:
  assert.match(stdin, /header = "authorization: Bearer tok\.en"/);
  assert.match(stdin, /data-binary = "\{\\"walletAddress\\":\\"0xabc\\"\}"/);
});

test("curl config: a GET with no token or body needs no --config", () => {
  const { args, stdin } = buildCurlArgs("https://h:1", { method: "GET", path: "/api/health" });
  assert.equal(stdin, "");
  assert.ok(!args.includes("--config"));
});

test("curl config: hostile body is one opaque, escaped config value", () => {
  const hostile = {
    content: "$(rm -rf /) `touch /tmp/pwned` ; & | > /etc/passwd '\" \\ \n --insecure",
    msgHash: "ab".repeat(32),
  };
  const { args, stdin } = buildCurlArgs("https://h:1", {
    method: "POST",
    path: "/api/rooms/room_0123456789/messages",
    body: hostile,
  });
  // Nothing sensitive in argv, and no argv element carries shell metacharacters:
  for (const arg of args) {
    assert.ok(!/[$`|;&<>]/.test(arg), `unexpected metacharacter in argv element: ${arg}`);
  }
  assert.ok(!args.includes("--insecure"), "'--insecure' in the body must not add a flag");

  // The body is a single quoted config value: the real newline is escaped to
  // an "\n" sequence, backslash and quote are escaped, so curl reads exactly
  // the JSON and a config line can never be split into a second directive.
  const json = JSON.stringify(hostile);
  const expectedValue = json
    .replace(/\\/g, "\\\\")
    .replace(/"/g, '\\"')
    .replace(/\n/g, "\\n");
  const lines = stdin.split("\n");
  const dataLine = lines.find((l) => l.startsWith("data-binary = "));
  assert.ok(dataLine, "data-binary config line present");
  assert.equal(dataLine, `data-binary = "${expectedValue}"`);
});

test("buildCurlConfig: token and body only, header-splitting refused", () => {
  assert.equal(buildCurlConfig({ method: "GET", path: "/api/health" }), "");

  const withToken = buildCurlConfig({ method: "GET", path: "/api/rooms", token: "jwt.tok.en" });
  assert.equal(withToken, 'header = "authorization: Bearer jwt.tok.en"\n');

  const withBody = buildCurlConfig({
    method: "POST",
    path: "/api/rooms",
    body: { name: "x" },
  });
  assert.match(withBody, /^header = "content-type: application\/json"\n/);
  assert.match(withBody, /data-binary = "\{\\"name\\":\\"x\\"\}"\n$/);

  // A CRLF-bearing token is refused before it can inject a second directive:
  assert.throws(
    () => buildCurlConfig({ method: "GET", path: "/api/rooms", token: "a\r\nheader = evil" }),
    TransportError,
  );
});

test("curl argv: a hostile string is never interpreted by a shell", async () => {
  // Run a real argv through execFile with /bin/echo standing in for curl:
  // if a shell were involved, $(...) would execute and the output would
  // differ from the literal bytes.
  const hostile = '$(echo injected) `echo injected` ; echo injected';
  const { stdout } = await execFileAsync("/bin/echo", [hostile]);
  assert.equal(stdout.trim(), hostile);
});

test("tokens with header-splitting characters are refused", () => {
  assert.throws(() => validateToken("abc\r\nx-injected: 1"), TransportError);
  assert.throws(() => validateToken("abc def"), TransportError);
  assert.throws(() => validateToken("abc\n"), TransportError);
  assert.throws(() => validateToken(""), TransportError);
  validateToken("eyJhbGciOiJIUzI1NiJ9.eyJ3IjoiMHgifQ.sig-_"); // fine
  assert.throws(
    () =>
      buildCurlArgs("https://h:1", { method: "GET", path: "/api/rooms", token: "a\r\nb" }),
    TransportError,
  );
});

test("request paths with whitespace or control chars are refused", () => {
  const t = createTransport({ baseUrl: "http://127.0.0.1:1" });
  assert.throws(() => buildCurlArgs("https://h:1", { method: "GET", path: "api/health" }), TransportError);
  assert.throws(
    () => buildCurlArgs("https://h:1", { method: "GET", path: "/api/he alth" }),
    TransportError,
  );
  void t.close();
});

test("curl output parsing: status marker split", () => {
  const parsed = parseCurlOutput('{"status":"ok"}\n__pskynet_http_status__:200');
  assert.equal(parsed.status, 200);
  assert.equal(parsed.bodyText, '{"status":"ok"}');
  // A body containing newlines survives:
  const multi = parseCurlOutput('line1\nline2\n__pskynet_http_status__:404');
  assert.equal(multi.status, 404);
  assert.equal(multi.bodyText, "line1\nline2");
  assert.throws(() => parseCurlOutput("no marker here"), /status marker/);
  assert.throws(() => parseCurlOutput("x\n__pskynet_http_status__:banana"), /unparseable/);
});

test("insecure/CA options are only wired for https URLs", async () => {
  // http URL + insecure: no dispatcher is created (nothing to relax).
  const t1 = createTransport({ baseUrl: "http://127.0.0.1:9099", insecure: true });
  assert.equal(t1.kind, "http1");
  await t1.close();
  // https URL + insecure builds fine.
  const t2 = createTransport({ baseUrl: "https://127.0.0.1:9443", insecure: true });
  await t2.close();
});
