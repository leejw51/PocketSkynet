import assert from "node:assert/strict";
import { test } from "node:test";
import * as secp from "@noble/secp256k1";
import {
  eip191Digest,
  parseSignature,
  personalSign,
  recoverAddress,
  verifyPersonalSign,
} from "../../src/eip191.js";
import { bytesToHex, hexToBytes, utf8Bytes } from "../../src/hex.js";
import { normalizePrivateKey } from "../../src/wallet.js";
import { protocolVectors } from "../helpers/vectors.js";

const vectors = protocolVectors();

test("eip191 vectors: digest, signature and recovery are byte-exact", () => {
  assert.ok(vectors.eip191.length >= 7, "expected the full eip191 vector set");
  for (const vector of vectors.eip191) {
    const priv = normalizePrivateKey(vector.privateKeyHex);

    const digest = bytesToHex(eip191Digest(vector.message));
    assert.equal(
      digest,
      vector.digestHex,
      `digest mismatch for ${vector.name}`,
    );

    const signature = personalSign(vector.message, priv);
    assert.equal(
      signature,
      vector.signatureHex,
      `signature mismatch for ${vector.name}`,
    );

    const recovered = recoverAddress(vector.message, vector.signatureHex);
    assert.equal(
      recovered,
      vector.address,
      `recovered address mismatch for ${vector.name}`,
    );

    assert.ok(verifyPersonalSign(vector.message, signature, vector.address));
    assert.ok(
      verifyPersonalSign(
        vector.message,
        signature,
        vector.address.toUpperCase().replace("0X", "0x"),
      ),
    );
  }
});

test("eip191 length prefix counts UTF-8 bytes, not characters", () => {
  const vector = vectors.eip191.find(
    (v) => v.name === "unicode-length-is-bytes",
  );
  assert.ok(vector, "unicode vector present");
  const bytes = utf8Bytes(vector.message);
  assert.equal(bytes.length, vector.messageUtf8Len);
  assert.notEqual([...vector.message].length, vector.messageUtf8Len);
  assert.equal(bytesToHex(eip191Digest(vector.message)), vector.digestHex);
});

test("signatures are always low-S with v in {27, 28}", () => {
  const halfN = secp.CURVE.n >> 1n;
  for (const vector of vectors.eip191) {
    const priv = normalizePrivateKey(vector.privateKeyHex);
    const signature = personalSign(vector.message, priv);
    assert.match(
      signature,
      /^0x[0-9a-f]{130}$/,
      "wire form is 0x + 130 lowercase hex",
    );
    const parsed = parseSignature(signature);
    assert.ok(parsed.s <= halfN, `s must be low for ${vector.name}`);
    const v = parseInt(signature.slice(-2), 16);
    assert.ok(v === 27 || v === 28, `v=${v} out of range for ${vector.name}`);
  }
});

test("signing is deterministic (RFC 6979): same input, same 65 bytes", () => {
  const vector = vectors.eip191[0]!;
  const priv = normalizePrivateKey(vector.privateKeyHex);
  const first = personalSign(vector.message, priv);
  const second = personalSign(vector.message, priv);
  assert.equal(first, second);
});

test("high-S signatures are rejected", () => {
  const vector = vectors.eip191[0]!;
  const bytes = hexToBytes(vector.signatureHex.slice(2));
  const r = secp.etc.bytesToNumberBE(bytes.subarray(0, 32));
  const s = secp.etc.bytesToNumberBE(bytes.subarray(32, 64));
  const highS = secp.CURVE.n - s; // the malleable twin
  const flippedV = bytes[64]! === 27 ? 28 : 27;
  const forged =
    "0x" +
    bytesToHex(secp.etc.numberToBytesBE(r)).padStart(64, "0") +
    bytesToHex(secp.etc.numberToBytesBE(highS)).padStart(64, "0") +
    flippedV.toString(16);
  assert.throws(() => parseSignature(forged), /high-S/);
  assert.equal(
    verifyPersonalSign(vector.message, forged, vector.address),
    false,
  );
});

test("v outside 27/28 (tolerating 0/1) is rejected", () => {
  const vector = vectors.eip191[0]!;
  const base = vector.signatureHex.slice(0, -2);
  // 0/1 tolerated on input:
  const vByte = parseInt(vector.signatureHex.slice(-2), 16) - 27;
  assert.equal(
    recoverAddress(vector.message, `${base}0${vByte}`),
    vector.address,
    "v of 0/1 must be tolerated on input",
  );
  for (const bad of ["02", "1d", "ff", "29"]) {
    assert.throws(() => parseSignature(`${base}${bad}`), /recovery byte/);
  }
});

test("malformed signatures are rejected", () => {
  const vector = vectors.eip191[0]!;
  const good = vector.signatureHex;
  assert.throws(() => parseSignature(good.slice(0, -2)), /130 hex/); // truncated
  assert.throws(() => parseSignature(good + "00"), /130 hex/); // extended
  assert.throws(() => parseSignature(good.replace("0x", "")), /130 hex/); // no prefix
  assert.throws(() => parseSignature("0x" + "zz".repeat(65)), /130 hex/); // non-hex
  const zeroR = "0x" + "00".repeat(32) + good.slice(66);
  assert.throws(() => parseSignature(zeroR), /r out of range/);
});

test("a tampered message no longer verifies", () => {
  const vector = vectors.eip191[0]!;
  assert.equal(
    verifyPersonalSign(
      vector.message + "!",
      vector.signatureHex,
      vector.address,
    ),
    false,
  );
  assert.equal(
    verifyPersonalSign(
      vector.message,
      vector.signatureHex,
      "0x" + "11".repeat(20),
    ),
    false,
  );
});

test("login-challenge vector matches the challenge template flow", () => {
  const vector = vectors.eip191.find((v) => v.name === "login-challenge");
  assert.ok(vector, "login-challenge vector present");
  assert.ok(vector.message.startsWith("Welcome to FruitNation!\n\n"));
  const priv = normalizePrivateKey(vector.privateKeyHex);
  assert.equal(personalSign(vector.message, priv), vector.signatureHex);
});
