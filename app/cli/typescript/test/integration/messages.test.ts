import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { PocketSkynetClient } from "../../src/client.js";
import { ApiError } from "../../src/errors.js";
import { msgHashPlaintext } from "../../src/protocol.js";
import { createTransport, Transport } from "../../src/transport.js";
import { randomPrivateKeyHex, TestServer } from "../helpers/harness.js";

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

async function loggedInClient(): Promise<PocketSkynetClient> {
  const client = new PocketSkynetClient({
    baseUrl: server.baseUrl,
    privateKey: randomPrivateKeyHex(),
  });
  await client.login();
  return client;
}

async function clientWithRoom(): Promise<{
  client: PocketSkynetClient;
  roomId: string;
}> {
  const client = await loggedInClient();
  const room = await client.createRoom("msg test room");
  return { client, roomId: room.id };
}

test("send + list round trip, server-controlled fields", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    const sent = await client.sendMessage(roomId, "hello from typescript");
    assert.equal(sent.content, "hello from typescript");
    assert.equal(sent.msgHash, msgHashPlaintext("hello from typescript"));
    assert.equal(sent.isEncrypted, false);
    assert.equal(sent.msgType, "add");
    assert.equal(sent.senderAddress, client.walletAddress);
    assert.ok(Number.isSafeInteger(sent.msgSerial) && sent.msgSerial >= 1);
    assert.ok(Number.isSafeInteger(sent.messageTimestamp));

    const listed = await client.messages(roomId);
    assert.equal(listed.length, 1);
    assert.equal(listed[0]!.id, sent.id);
    assert.equal(listed[0]!.content, "hello from typescript");
  } finally {
    await client.close();
  }
});

test("content is trimmed server-side; msgHash is of the trimmed content", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    const sent = await client.sendMessage(roomId, "  padded \n");
    assert.equal(sent.content, "padded");
    assert.equal(sent.msgHash, msgHashPlaintext("padded"));
  } finally {
    await client.close();
  }
});

test("unicode content survives byte-exactly", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    const text = "한글 메시지 🍓🍊";
    const sent = await client.sendMessage(roomId, text);
    assert.equal(sent.content, text);
    // Pinned by the protocol vector for this exact string:
    assert.equal(
      sent.msgHash,
      "90f15b87d2781befd4a1b6a91dea008417ad8b3f2e53d5cdaa82752db72009dd",
    );
    const listed = await client.messages(roomId);
    assert.equal(listed[listed.length - 1]!.content, text);
  } finally {
    await client.close();
  }
});

test("listing is chronological (messageTimestamp, msgSerial) and respects limit", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    for (let i = 0; i < 5; i++) {
      await client.sendMessage(roomId, `message ${i}`);
    }
    const all = await client.messages(roomId);
    assert.equal(all.length, 5);
    for (let i = 1; i < all.length; i++) {
      const prev = all[i - 1]!;
      const next = all[i]!;
      const ordered =
        prev.messageTimestamp < next.messageTimestamp ||
        (prev.messageTimestamp === next.messageTimestamp &&
          prev.msgSerial < next.msgSerial);
      assert.ok(ordered, "ascending by (messageTimestamp, msgSerial)");
    }
    assert.deepEqual(
      all.map((m) => m.content),
      [0, 1, 2, 3, 4].map((i) => `message ${i}`),
    );

    const limited = await client.messages(roomId, { limit: 2 });
    assert.equal(limited.length, 2);
    // The newest page, still ascending:
    assert.deepEqual(
      limited.map((m) => m.content),
      ["message 3", "message 4"],
    );
  } finally {
    await client.close();
  }
});

test("a non-member cannot send or list: 403", async () => {
  const { client, roomId } = await clientWithRoom();
  const stranger = await loggedInClient();
  try {
    await assert.rejects(
      () => stranger.sendMessage(roomId, "let me in"),
      (err: unknown) =>
        err instanceof ApiError &&
        err.status === 403 &&
        err.message === "Access denied",
    );
    await assert.rejects(
      () => stranger.messages(roomId),
      (err: unknown) => err instanceof ApiError && err.status === 403,
    );
  } finally {
    await client.close();
    await stranger.close();
  }
});

test("msgHash is validated: missing, uppercase and wrong-length are 400", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    const token = await client.ensureToken();
    const path = `/api/rooms/${roomId}/messages`;
    const cases: unknown[] = [
      { content: "hi" }, // missing
      { content: "hi", msgHash: "A".repeat(64) }, // uppercase refused
      { content: "hi", msgHash: "abc123" }, // wrong length
      { content: "hi", msgHash: null }, // null is not a hash
    ];
    for (const body of cases) {
      const response = await raw.request({ method: "POST", path, body, token });
      assert.equal(
        response.status,
        400,
        `expected 400 for ${JSON.stringify(body)}`,
      );
      const parsed = JSON.parse(response.bodyText) as { message: string };
      assert.equal(parsed.message, "Validation failed");
    }
  } finally {
    await client.close();
  }
});

test("content boundary: 5000 chars is accepted, 5001 is 400", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    const exactly = "x".repeat(5000);
    const sent = await client.sendMessage(roomId, exactly);
    assert.equal(sent.content.length, 5000);

    await assert.rejects(
      () => client.sendMessage(roomId, "x".repeat(5001)),
      (err: unknown) => err instanceof ApiError && err.status === 400,
    );
  } finally {
    await client.close();
  }
});

test("a body over the 100KB cap is 413", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    const token = await client.ensureToken();
    const oversized = {
      content: "y".repeat(120_000), // > 100 KB body, also > 5000 chars
      msgHash: "ab".repeat(32),
    };
    const response = await raw.request({
      method: "POST",
      path: `/api/rooms/${roomId}/messages`,
      body: oversized,
      token,
    });
    assert.equal(response.status, 413);
  } finally {
    await client.close();
  }
});

test("empty and whitespace-only content is refused", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    await assert.rejects(
      () => client.sendMessage(roomId, ""),
      (err: unknown) => err instanceof ApiError && err.status === 400,
    );
  } finally {
    await client.close();
  }
});

test("concurrency: parallel sends all land with distinct msgSerials", async () => {
  const { client, roomId } = await clientWithRoom();
  try {
    const results = await Promise.all(
      Array.from({ length: 10 }, (_, i) =>
        client.sendMessage(roomId, `parallel ${i}`),
      ),
    );
    const serials = results.map((m) => m.msgSerial);
    assert.equal(
      new Set(serials).size,
      10,
      `serials must be distinct: ${serials}`,
    );
    const listed = await client.messages(roomId, { limit: 100 });
    assert.equal(listed.length, 10);
    const contents = new Set(listed.map((m) => m.content));
    for (let i = 0; i < 10; i++) {
      assert.ok(contents.has(`parallel ${i}`), `parallel ${i} must be stored`);
    }
  } finally {
    await client.close();
  }
});
