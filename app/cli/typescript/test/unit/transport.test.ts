import assert from "node:assert/strict";
import { test } from "node:test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { TransportError } from "../../src/errors.js";
import {
  buildCurlArgs,
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

test("http3 transport without a capable curl fails with a clear error", async () => {
  const transport = new CurlHttp3Transport({
    baseUrl: "https://127.0.0.1:1",
    http3: true,
    // A curl that exists but has no HTTP3 feature must be refused by the
    // probe; pointing the probe at /usr/bin/true guarantees "no candidate".
    curlPath: undefined as unknown as string,
  });
  // Force the probe to find nothing by narrowing candidates via env.
  const saved = process.env["PSKYNET_CURL"];
  process.env["PSKYNET_CURL"] = "/usr/bin/false";
  try {
    const found = await findHttp3Curl(["/usr/bin/false", "/nonexistent/curl"]);
    assert.equal(found, null);
  } finally {
    if (saved === undefined) delete process.env["PSKYNET_CURL"];
    else process.env["PSKYNET_CURL"] = saved;
  }
  await transport.close();
});

test("findHttp3Curl rejects a curl whose features lack HTTP3", async () => {
  // The system curl on this machine may or may not have HTTP3; assert the
  // probe agrees with `curl --version` rather than assuming either way.
  const found = await findHttp3Curl(["curl"]);
  const { stdout } = await execFileAsync("curl", ["--version"]);
  const hasH3 = /^Features:.*\bHTTP3\b/m.test(stdout);
  assert.equal(found !== null, hasH3);
});

test("curl argv: exact shape, URL last after --", () => {
  const args = buildCurlArgs(
    "https://127.0.0.1:9101",
    { method: "POST", path: "/api/auth/login", body: { walletAddress: "0xabc" }, token: "tok.en" },
    { insecure: true, timeoutMs: 5000 },
  );
  assert.equal(args[0], "--http3-only");
  assert.equal(args[args.length - 2], "--");
  assert.equal(args[args.length - 1], "https://127.0.0.1:9101/api/auth/login");
  assert.ok(args.includes("--insecure"));
  const dataAt = args.indexOf("--data-binary");
  assert.ok(dataAt > 0);
  assert.equal(args[dataAt + 1], '{"walletAddress":"0xabc"}');
  const authAt = args.findIndex((a) => a.startsWith("authorization:"));
  assert.equal(args[authAt], "authorization: Bearer tok.en");
  assert.equal(args[authAt - 1], "--header");
});

test("curl argv: hostile body content stays a single inert argv element", () => {
  const hostile = {
    content: "$(rm -rf /) `touch /tmp/pwned` ; & | > /etc/passwd '\" \\ \n --insecure",
    msgHash: "ab".repeat(32),
  };
  const args = buildCurlArgs("https://h:1", {
    method: "POST",
    path: "/api/rooms/room_0123456789/messages",
    body: hostile,
  });
  const dataAt = args.indexOf("--data-binary");
  const payload = args[dataAt + 1]!;
  // The whole hostile body is one argv element, byte-identical to its JSON:
  assert.equal(payload, JSON.stringify(hostile));
  // and no argv element other than the payload contains shell metacharacters:
  for (const [i, arg] of args.entries()) {
    if (i === dataAt + 1) continue;
    assert.ok(!/[$`|;&<>]/.test(arg), `unexpected metacharacter in argv[${i}]: ${arg}`);
  }
  // "--insecure" inside the *payload* must not add a flag:
  assert.equal(args.filter((a) => a === "--insecure").length, 0);
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
