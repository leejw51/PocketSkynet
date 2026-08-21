import assert from "node:assert/strict";
import { createServer, Server } from "node:http";
import { AddressInfo } from "node:net";
import { after, test } from "node:test";
import { PocketSkynetClient } from "../../src/client.js";
import { TransportError } from "../../src/errors.js";

// A throwaway private key (Hardhat #0) — these tests never reach a real server.
const KEY =
  "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

test("ensureToken dedups parallel first logins: one challenge burned", async () => {
  const client = new PocketSkynetClient({
    baseUrl: "http://127.0.0.1:1",
    privateKey: KEY,
  });
  let loginCalls = 0;
  // Replace the real login with a slow stub that records how often it runs.
  (client as unknown as { login: () => Promise<unknown> }).login = async () => {
    loginCalls += 1;
    await new Promise((r) => setTimeout(r, 20));
    (client as unknown as { jwt: string }).jwt = "minted-token";
    return {};
  };

  const tokens = await Promise.all([
    client.ensureToken(),
    client.ensureToken(),
    client.ensureToken(),
  ]);
  assert.deepEqual(tokens, ["minted-token", "minted-token", "minted-token"]);
  assert.equal(
    loginCalls,
    1,
    "three parallel first calls must share one login",
  );

  // After it resolves the in-flight slot is cleared; a later call with the
  // token already cached does not log in again.
  await client.ensureToken();
  assert.equal(loginCalls, 1);
  await client.close();
});

test("ensureToken lets a fresh login run after a failed one", async () => {
  const client = new PocketSkynetClient({
    baseUrl: "http://127.0.0.1:1",
    privateKey: KEY,
  });
  let loginCalls = 0;
  (client as unknown as { login: () => Promise<unknown> }).login = async () => {
    loginCalls += 1;
    if (loginCalls === 1) throw new Error("boom");
    (client as unknown as { jwt: string }).jwt = "ok";
    return {};
  };
  await assert.rejects(() => client.ensureToken(), /boom/);
  // The failed in-flight login must not be sticky:
  assert.equal(await client.ensureToken(), "ok");
  assert.equal(loginCalls, 2);
  await client.close();
});

test("fetch transport caps an oversized response body", async () => {
  // A server that streams past the 32 MB ceiling.
  const chunk = Buffer.alloc(1024 * 1024, 0x61); // 1 MB of 'a'
  const server: Server = createServer((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    let sent = 0;
    const pump = (): void => {
      while (sent < 40) {
        sent += 1;
        if (!res.write(chunk)) {
          res.once("drain", pump);
          return;
        }
      }
      res.end();
    };
    pump();
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as AddressInfo).port;
  const client = new PocketSkynetClient({
    baseUrl: `http://127.0.0.1:${port}`,
  });
  try {
    await assert.rejects(
      () => client.health(),
      (err: unknown) =>
        err instanceof TransportError && /exceeded .* bytes/.test(err.message),
    );
  } finally {
    await client.close();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});

after(() => {
  // Nothing global to clean up; per-test teardown handles servers.
});
