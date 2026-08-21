import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { PocketSkynetClient, buildLoginBody } from "../../src/client.js";
import { personalSign } from "../../src/eip191.js";
import { ApiError } from "../../src/errors.js";
import { createTransport, Transport } from "../../src/transport.js";
import type { ChallengeResponse, LoginResponse } from "../../src/types.js";
import { accountFromPrivateKey, normalizePrivateKey } from "../../src/wallet.js";
import { JWT_SECRET, randomPrivateKeyHex, TestServer } from "../helpers/harness.js";
import { mintJwt, tamperSignature } from "../helpers/jwt.js";

let server: TestServer;
let raw: Transport;

before(async () => {
  server = await TestServer.start();
  raw = createTransport({ baseUrl: server.baseUrl });
});

after(async () => {
  await raw?.close();
  await server?.stop();
});

function newClient(username?: string): PocketSkynetClient {
  const opts: ConstructorParameters<typeof PocketSkynetClient>[0] = {
    baseUrl: server.baseUrl,
    privateKey: randomPrivateKeyHex(),
  };
  if (username !== undefined) opts.username = username;
  return new PocketSkynetClient(opts);
}

test("health answers without auth", async () => {
  const client = newClient();
  const health = await client.health();
  assert.equal(health.status, "ok");
  assert.equal(typeof health.uptime, "number");
  await client.close();
});

test("login happy path with an explicit username", async () => {
  const client = newClient("alice_ts");
  const result = await client.login();
  assert.equal(result.retriedWithGeneratedUsername, false);
  assert.equal(result.usernameSent, "alice_ts");
  assert.equal(result.response.user.username, "alice_ts");
  assert.equal(result.response.user.walletAddress, result.walletAddress);
  assert.match(result.walletAddress, /^0x[0-9a-f]{40}$/, "wire addresses are lowercase");
  assert.ok(result.response.token.split(".").length === 3, "JWT shaped token");
  assert.match(result.response.encryptionSalt ?? "", /^[0-9a-f]{64}$/);
  assert.equal(client.token, result.response.token);
  await client.close();
});

test("first-time login without a username retries once with a generated one", async () => {
  const client = newClient();
  const result = await client.login();
  assert.equal(result.retriedWithGeneratedUsername, true);
  assert.ok(result.usernameSent !== undefined);
  assert.equal(result.response.user.username, result.usernameSent);
  await client.close();
});

test("second login of a known account reuses the stored username", async () => {
  const key = randomPrivateKeyHex();
  const first = new PocketSkynetClient({
    baseUrl: server.baseUrl,
    privateKey: key,
    username: "returning_user",
  });
  await first.login();
  await first.close();

  // No username this time: server must reuse "returning_user".
  const second = new PocketSkynetClient({ baseUrl: server.baseUrl, privateKey: key });
  const result = await second.login();
  assert.equal(result.retriedWithGeneratedUsername, false);
  assert.equal(result.usernameSent, undefined);
  assert.equal(result.response.user.username, "returning_user");
  await second.close();
});

test("wrong signature is 401 Invalid signature", async () => {
  const accountA = accountFromPrivateKey(randomPrivateKeyHex());
  const keyB = accountFromPrivateKey(randomPrivateKeyHex()).privateKey;

  const challengeResponse = await raw.request({
    method: "POST",
    path: "/api/auth/challenge",
    body: { walletAddress: accountA.address },
  });
  assert.equal(challengeResponse.status, 200);
  const challenge = JSON.parse(challengeResponse.bodyText) as ChallengeResponse;

  // Sign the right message with the WRONG key: recovers to someone else.
  const signature = personalSign(challenge.message, keyB);
  const login = await raw.request({
    method: "POST",
    path: "/api/auth/login",
    body: buildLoginBody({
      walletAddress: accountA.address,
      challengeId: challenge.challengeId,
      signature,
      username: "mallory",
    }),
  });
  assert.equal(login.status, 401);
  assert.equal((JSON.parse(login.bodyText) as { message: string }).message, "Invalid signature");
});

test("a challenge is burned by success: replay is 400", async () => {
  const key = randomPrivateKeyHex();
  const account = accountFromPrivateKey(key);
  const challengeResponse = await raw.request({
    method: "POST",
    path: "/api/auth/challenge",
    body: { walletAddress: account.address },
  });
  const challenge = JSON.parse(challengeResponse.bodyText) as ChallengeResponse;
  const signature = personalSign(challenge.message, account.privateKey);
  const body = buildLoginBody({
    walletAddress: account.address,
    challengeId: challenge.challengeId,
    signature,
    username: "replay_victim",
  });

  const first = await raw.request({ method: "POST", path: "/api/auth/login", body });
  assert.equal(first.status, 200, first.bodyText);

  const replay = await raw.request({ method: "POST", path: "/api/auth/login", body });
  assert.equal(replay.status, 400);
  assert.equal(
    (JSON.parse(replay.bodyText) as { message: string }).message,
    "Invalid or expired challenge",
  );
});

test("a challenge is burned by failure too", async () => {
  const key = randomPrivateKeyHex();
  const account = accountFromPrivateKey(key);
  const challengeResponse = await raw.request({
    method: "POST",
    path: "/api/auth/challenge",
    body: { walletAddress: account.address },
  });
  const challenge = JSON.parse(challengeResponse.bodyText) as ChallengeResponse;

  // Fail once with a garbage-but-well-formed signature (wrong key).
  const wrong = personalSign(challenge.message, normalizePrivateKey(randomPrivateKeyHex()));
  const failed = await raw.request({
    method: "POST",
    path: "/api/auth/login",
    body: buildLoginBody({
      walletAddress: account.address,
      challengeId: challenge.challengeId,
      signature: wrong,
      username: "x_y_z",
    }),
  });
  assert.equal(failed.status, 401);

  // Now the RIGHT signature against the same challenge must be refused.
  const right = personalSign(challenge.message, account.privateKey);
  const retry = await raw.request({
    method: "POST",
    path: "/api/auth/login",
    body: buildLoginBody({
      walletAddress: account.address,
      challengeId: challenge.challengeId,
      signature: right,
      username: "x_y_z",
    }),
  });
  assert.equal(retry.status, 400);
  assert.equal(
    (JSON.parse(retry.bodyText) as { message: string }).message,
    "Invalid or expired challenge",
  );
});

test("a token minted with the known test secret is accepted", async () => {
  const client = newClient("minted_user");
  const login = await client.login();
  await client.close();

  const minted = mintJwt(JWT_SECRET, { walletAddress: login.walletAddress });
  const rooms = await raw.request({ method: "GET", path: "/api/rooms", token: minted });
  assert.equal(rooms.status, 200, rooms.bodyText);
});

test("tampered JWT signature is 401", async () => {
  const client = newClient("tamper_user");
  const login = await client.login();
  await client.close();

  const bad = tamperSignature(login.response.token);
  const response = await raw.request({ method: "GET", path: "/api/rooms", token: bad });
  assert.equal(response.status, 401);
});

test("expired JWT (minted with the real secret) is 401", async () => {
  const client = newClient("expired_user");
  const login = await client.login();
  await client.close();

  const past = Math.floor(Date.now() / 1000) - 3600;
  const expired = mintJwt(JWT_SECRET, {
    walletAddress: login.walletAddress,
    iat: past - 60,
    exp: past,
  });
  const response = await raw.request({ method: "GET", path: "/api/rooms", token: expired });
  assert.equal(response.status, 401);
});

test("a JWT signed with the wrong secret is 401", async () => {
  const forged = mintJwt("not-the-real-secret-aaaaaaaaaaaaaaaaaaaaaaaa", {
    walletAddress: "0x" + "11".repeat(20),
  });
  const response = await raw.request({ method: "GET", path: "/api/rooms", token: forged });
  assert.equal(response.status, 401);
});

test("no token at all is 401", async () => {
  const response = await raw.request({ method: "GET", path: "/api/rooms" });
  assert.equal(response.status, 401);
});

test("client surfaces API errors as ApiError with status and message", async () => {
  const client = new PocketSkynetClient({ baseUrl: server.baseUrl, token: "garbage.token.here" });
  await assert.rejects(
    () => client.rooms(),
    (err: unknown) => err instanceof ApiError && err.status === 401,
  );
  await client.close();
});
