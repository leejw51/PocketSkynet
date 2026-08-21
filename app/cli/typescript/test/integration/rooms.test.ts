import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { PocketSkynetClient } from "../../src/client.js";
import { ApiError } from "../../src/errors.js";
import { randomPrivateKeyHex, TestServer } from "../helpers/harness.js";

let server: TestServer;

before(async () => {
  server = await TestServer.start();
});

after(async () => {
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

test("a fresh account is a member of the three built-in rooms", async () => {
  const client = await loggedInClient();
  try {
    const rooms = await client.rooms();
    const names = rooms.map((r) => r.name);
    // Assert membership, never counts — deployments may add rooms.
    for (const expected of ["My Note", "My Jarvis", "My Lobby"]) {
      assert.ok(names.includes(expected), `expected built-in room ${expected} in ${names}`);
    }
    const kinds = new Set(rooms.map((r) => r.kind));
    for (const kind of ["note", "jarvis", "lobby"]) {
      assert.ok(kinds.has(kind), `expected a room of kind ${kind}`);
    }
  } finally {
    await client.close();
  }
});

test("create-room round trip: bare Room back, then visible in the list", async () => {
  const client = await loggedInClient();
  try {
    const room = await client.createRoom("Team chat", "the team channel");
    assert.ok(room.id.length >= 10);
    assert.equal(room.name, "Team chat");
    assert.equal(room.kind, "channel");
    // Creation returns the bare room — no members enrichment:
    assert.equal((room as { members?: unknown }).members, undefined);

    const rooms = await client.rooms();
    const found = rooms.find((r) => r.id === room.id);
    assert.ok(found, "created room must appear in GET /api/rooms");
    assert.equal(found.name, "Team chat");

    // And the single-room endpoint returns the enriched shape:
    const enriched = await client.room(room.id);
    assert.ok(Array.isArray(enriched.members));
  } finally {
    await client.close();
  }
});

test("room names are validated: forbidden characters are 400 with the errors array", async () => {
  const client = await loggedInClient();
  try {
    await assert.rejects(
      () => client.createRoom("<script>alert(1)</script>"),
      (err: unknown) =>
        err instanceof ApiError &&
        err.status === 400 &&
        err.message === "Validation failed" &&
        Array.isArray(err.errors) &&
        err.errors.length > 0,
    );
    await assert.rejects(
      () => client.createRoom(""),
      (err: unknown) => err instanceof ApiError && err.status === 400,
    );
  } finally {
    await client.close();
  }
});

test("unicode room names survive the round trip", async () => {
  const client = await loggedInClient();
  try {
    const room = await client.createRoom("한글 방 🍓");
    assert.equal(room.name, "한글 방 🍓");
    const rooms = await client.rooms();
    assert.ok(rooms.some((r) => r.id === room.id && r.name === "한글 방 🍓"));
  } finally {
    await client.close();
  }
});

test("a foreign room is 403 Access denied", async () => {
  const owner = await loggedInClient();
  const stranger = await loggedInClient();
  try {
    const room = await owner.createRoom("Private club");
    await assert.rejects(
      () => stranger.room(room.id),
      (err: unknown) =>
        err instanceof ApiError && err.status === 403 && err.message === "Access denied",
    );
  } finally {
    await owner.close();
    await stranger.close();
  }
});

test("a NONEXISTENT room is also 403, not 404 (no existence oracle)", async () => {
  const client = await loggedInClient();
  try {
    await assert.rejects(
      () => client.room("room_does_not_exist_123456"),
      (err: unknown) =>
        err instanceof ApiError && err.status === 403 && err.message === "Access denied",
    );
  } finally {
    await client.close();
  }
});

test("a malformed room id is 400, not 403", async () => {
  const client = await loggedInClient();
  try {
    await assert.rejects(
      // '!' is outside [a-zA-Z0-9_.-]:
      () => client.room("bad!room!id!!!"),
      (err: unknown) => err instanceof ApiError && err.status === 400,
    );
  } finally {
    await client.close();
  }
});
