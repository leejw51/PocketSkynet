import assert from "node:assert/strict";
import { test } from "node:test";
import { buildLoginBody, parseJsonBody } from "../../src/client.js";
import { ApiError, apiErrorFromBody } from "../../src/errors.js";
import type { LoginResponse, MessageWithSender, RoomWithMembers } from "../../src/types.js";

test("login body: username present when given, camelCase keys", () => {
  const body = buildLoginBody({
    walletAddress: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
    challengeId: "6f1e2c30-aaaa-bbbb-cccc-ddddeeeeffff",
    signature: "0x" + "ab".repeat(65),
    username: "alice",
  });
  assert.deepEqual(Object.keys(body).sort(), [
    "challengeId",
    "signature",
    "username",
    "walletAddress",
  ]);
  assert.equal(body.username, "alice");
});

test("login body: undefined username is OMITTED from the JSON, never null", () => {
  const body = buildLoginBody({
    walletAddress: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
    challengeId: "id",
    signature: "0xab",
  });
  assert.ok(!("username" in body), "key must not exist on the object");
  const wire = JSON.stringify(body);
  assert.ok(!wire.includes("username"), "serialized body must not mention username");
  assert.ok(!wire.includes("null"), "no null anywhere in the body");
  assert.deepEqual(JSON.parse(wire), {
    walletAddress: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
    challengeId: "id",
    signature: "0xab",
  });
});

test("login body: empty-string username is treated as absent", () => {
  const body = buildLoginBody({
    walletAddress: "0x" + "aa".repeat(20),
    challengeId: "id",
    signature: "0xab",
    username: "",
  });
  assert.ok(!("username" in body));
});

test("response parsing tolerates nulls and unknown fields", () => {
  const login = parseJsonBody<LoginResponse>(
    JSON.stringify({
      user: {
        walletAddress: "0xabc",
        username: null,
        publicKey: null,
        someFutureField: { nested: true },
      },
      token: "jwt",
      fruitnationWallet: "0xServer",
      encryptionSalt: "ab".repeat(32),
      anotherUnknown: 42,
    }),
  );
  assert.equal(login.token, "jwt");
  assert.equal(login.user.username, null);
  assert.equal(login.user["someFutureField" as keyof typeof login.user] !== undefined, true);

  const room = parseJsonBody<RoomWithMembers>(
    JSON.stringify({
      id: "room_123456",
      name: "My Note",
      description: null,
      kind: "note",
      unknownEnrichment: [],
    }),
  );
  assert.equal(room.description, null);
  assert.equal(room.lastMessage, undefined); // omitted, not null

  const message = parseJsonBody<MessageWithSender>(
    JSON.stringify({
      id: "msg_1",
      roomId: "room_123456",
      senderAddress: "0xabc",
      content: "hi",
      msgHash: "00".repeat(32),
      isEncrypted: false,
      iv: null,
      hmac: null,
      txHash: null,
      msgSerial: 7,
      messageTimestamp: 1749652739650,
    }),
  );
  assert.equal(message.iv, null);
  assert.equal(message.replyCount, undefined); // absent when there are no replies
  assert.equal(message.msgSerial, 7);
});

test("non-JSON success bodies raise a useful error", () => {
  assert.throws(() => parseJsonBody("<html>gateway error</html>"), /non-JSON body/);
  assert.equal(parseJsonBody<undefined>(""), undefined);
});

test("error envelope shape 1: message only", () => {
  const err = apiErrorFromBody(403, JSON.stringify({ message: "Access denied" }));
  assert.ok(err instanceof ApiError);
  assert.equal(err.status, 403);
  assert.equal(err.message, "Access denied");
  assert.equal(err.errors, undefined);
  assert.equal(err.code, undefined);
});

test("error envelope shape 2: validation failure with errors array", () => {
  const err = apiErrorFromBody(
    400,
    JSON.stringify({
      message: "Validation failed",
      errors: ["roomId: Room ID contains invalid characters"],
    }),
  );
  assert.equal(err.status, 400);
  assert.equal(err.message, "Validation failed");
  assert.deepEqual(err.errors, ["roomId: Room ID contains invalid characters"]);
});

test("error envelope shape 3: machine-readable code with currentKeyVersion", () => {
  const err = apiErrorFromBody(
    409,
    JSON.stringify({
      code: "KEY_ROTATION_REQUIRED",
      message: "Room key rotation is pending",
      currentKeyVersion: 3,
    }),
  );
  assert.equal(err.status, 409);
  assert.equal(err.code, "KEY_ROTATION_REQUIRED");
  assert.equal(err.currentKeyVersion, 3);
});

test("error parsing survives non-JSON and wrong-shaped bodies", () => {
  const html = apiErrorFromBody(502, "<html>bad gateway</html>");
  assert.equal(html.status, 502);
  assert.ok(html.message.includes("bad gateway"));

  const empty = apiErrorFromBody(500, "");
  assert.equal(empty.message, "HTTP 500");

  const wrongShape = apiErrorFromBody(500, JSON.stringify({ message: 42, errors: "nope" }));
  assert.equal(wrongShape.message, "HTTP 500");
  assert.equal(wrongShape.errors, undefined);

  const mixedErrors = apiErrorFromBody(
    400,
    JSON.stringify({ message: "Validation failed", errors: ["ok", 42, null] }),
  );
  assert.deepEqual(mixedErrors.errors, ["ok"]);
});
