/**
 * HTTP/3 end-to-end, via the curl-based transport.
 *
 * Node 24 ships no HTTP/3 client (`node:quic` is not exposed even behind
 * `--experimental-quic`) and npm has no maintained pure-JS HTTP/3 client, so
 * the transport shells out to an HTTP/3-capable curl. When the machine has no
 * such curl the whole group SKIPs — loudly, with the reason — and one test
 * still pins the fail-fast error message.
 */

import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { PocketSkynetClient } from "../../src/client.js";
import { TransportError } from "../../src/errors.js";
import { CurlHttp3Transport, findHttp3Curl } from "../../src/transport.js";
import { randomPrivateKeyHex, TestServer } from "../helpers/harness.js";

let server: TestServer | undefined;

// Probed at module load (top-level await) so the `skip` options below see the
// real answer when the tests are registered.
const curlPath: string | null = await findHttp3Curl();

before(async () => {
  if (curlPath !== null) {
    server = await TestServer.start({ http3: true });
  }
});

after(async () => {
  await server?.stop();
});

const SKIP_REASON =
  "no HTTP/3-capable curl on this machine (checked PSKYNET_CURL, Homebrew curl, PATH curl)";

test("http3: health over QUIC", { skip: curlPathMissing() }, async () => {
  const transport = h3Transport();
  const response = await transport.request({ method: "GET", path: "/api/health" });
  assert.equal(response.status, 200);
  assert.equal((JSON.parse(response.bodyText) as { status: string }).status, "ok");
  await transport.close();
});

test("http3: full auth flow and room create over QUIC", { skip: curlPathMissing() }, async () => {
  const client = new PocketSkynetClient({
    baseUrl: server!.http3Url(""),
    http3: true,
    curlPath: curlPath!,
    caPath: server!.caPath(),
    privateKey: randomPrivateKeyHex(),
  });
  try {
    const login = await client.login();
    assert.ok(login.response.token.length > 0);
    const room = await client.createRoom("h3 room");

    // The room created over HTTP/3 must be visible over plain TCP too:
    const tcp = new PocketSkynetClient({
      baseUrl: server!.baseUrl,
      token: login.response.token,
    });
    try {
      const rooms = await tcp.rooms();
      assert.ok(rooms.some((r) => r.id === room.id));
    } finally {
      await tcp.close();
    }

    const sent = await client.sendMessage(room.id, "over quic 🛰️");
    assert.equal(sent.content, "over quic 🛰️");
    const listed = await client.messages(room.id);
    assert.equal(listed[listed.length - 1]!.content, "over quic 🛰️");
  } finally {
    await client.close();
  }
});

test("http3 transport fails fast with a clear error when curl is unavailable", async () => {
  // `curlPath: null` means "known unavailable", so this exercises the
  // fail-fast path unconditionally — even on machines that DO have an
  // HTTP/3-capable curl, where a probe-based test would silently skip it.
  const forced = new CurlHttp3Transport({
    baseUrl: "https://127.0.0.1:1",
    http3: true,
    curlPath: null,
  });
  try {
    await assert.rejects(
      () => forced.request({ method: "GET", path: "/api/health" }),
      (err: unknown) =>
        err instanceof TransportError && /HTTP\/3-capable curl/.test(err.message),
    );
  } finally {
    await forced.close();
  }
});

function curlPathMissing(): string | boolean {
  // node:test evaluates `skip` at test time; curlPath is set in before().
  return curlPath === null ? SKIP_REASON : false;
}

function h3Transport(): CurlHttp3Transport {
  return new CurlHttp3Transport({
    baseUrl: server!.http3Url(""),
    http3: true,
    curlPath: curlPath!,
    caPath: server!.caPath(),
  });
}
