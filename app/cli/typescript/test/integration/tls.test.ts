import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { PocketSkynetClient } from "../../src/client.js";
import { TransportError } from "../../src/errors.js";
import { randomPrivateKeyHex, TestServer } from "../helpers/harness.js";

let server: TestServer;

before(async () => {
  server = await TestServer.start({ tls: true });
});

after(async () => {
  await server?.stop();
});

test("HTTPS with --insecure works against the self-signed dev cert", async () => {
  const client = new PocketSkynetClient({
    baseUrl: server.baseUrl,
    insecure: true,
    privateKey: randomPrivateKeyHex(),
  });
  try {
    const health = await client.health();
    assert.equal(health.status, "ok");
    const login = await client.login();
    assert.ok(login.response.token.length > 0);
    const rooms = await client.rooms();
    assert.ok(rooms.some((r) => r.name === "My Note"));
  } finally {
    await client.close();
  }
});

test("HTTPS pinning the server's generated CA works (the preferred path)", async () => {
  const client = new PocketSkynetClient({
    baseUrl: server.baseUrl,
    caPath: server.caPath(),
    privateKey: randomPrivateKeyHex(),
  });
  try {
    const health = await client.health();
    assert.equal(health.status, "ok");
    await client.login();
  } finally {
    await client.close();
  }
});

test("HTTPS WITHOUT --insecure or a CA is refused, not silently accepted", async () => {
  const client = new PocketSkynetClient({ baseUrl: server.baseUrl });
  try {
    await assert.rejects(
      () => client.health(),
      (err: unknown) => err instanceof TransportError,
      "an unknown self-signed certificate must fail TLS verification",
    );
  } finally {
    await client.close();
  }
});
