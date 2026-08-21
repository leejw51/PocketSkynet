import assert from "node:assert/strict";
import { test } from "node:test";
import { generatedUsername, msgHashCiphertext, msgHashPlaintext } from "../../src/protocol.js";
import { protocolVectors } from "../helpers/vectors.js";

const vectors = protocolVectors();

test("msgHash.plaintext vectors: SHA-256 of the trimmed content", () => {
  assert.ok(vectors.msgHash.plaintext.length >= 3);
  for (const vector of vectors.msgHash.plaintext) {
    assert.equal(msgHashPlaintext(vector.content), vector.msgHashHex);
    if (vector.trimmedTo !== undefined) {
      // The hash of the untrimmed and trimmed forms must agree — trim first.
      assert.equal(msgHashPlaintext(vector.trimmedTo), vector.msgHashHex);
    }
  }
});

test("msgHash trims before hashing (whitespace-wrapped unicode)", () => {
  assert.equal(msgHashPlaintext("  hi \n"), msgHashPlaintext("hi"));
  assert.notEqual(msgHashPlaintext("hi"), msgHashPlaintext("ho"));
  const unicode = vectors.msgHash.plaintext.find((v) => /[^\x00-\x7f]/.test(v.content));
  assert.ok(unicode, "unicode plaintext vector present");
  assert.equal(msgHashPlaintext(unicode.content), unicode.msgHashHex);
});

test("msgHash.encrypted vectors: SHA-256 of the base64 string, padding included", () => {
  assert.ok(vectors.msgHash.encrypted.length >= 3);
  for (const vector of vectors.msgHash.encrypted) {
    assert.equal(msgHashCiphertext(vector.ciphertextBase64), vector.msgHashHex);
  }
});

test("msgHash output shape is 64 lowercase hex", () => {
  assert.match(msgHashPlaintext("abc"), /^[a-f0-9]{64}$/);
});

test("generated usernames satisfy the server's username schema", () => {
  for (let i = 0; i < 50; i++) {
    const name = generatedUsername("0xF39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    assert.ok(name.length >= 3 && name.length <= 100);
    assert.ok(!/[<>{};"'`\\,]/.test(name), "no forbidden characters");
    // eslint-disable-next-line no-control-regex
    assert.ok(!/[\x00-\x1f\x7f]/.test(name), "no control characters");
  }
});
