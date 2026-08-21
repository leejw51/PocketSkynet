import assert from "node:assert/strict";
import { test } from "node:test";
import { bytesToHex } from "../../src/hex.js";
import {
  accountFromMnemonic,
  accountFromPrivateKey,
  mnemonicToSeed,
  normalizePrivateKey,
  privateKeyToHex,
  publicKeyFromPrivate,
  toChecksumAddress,
} from "../../src/wallet.js";
import { protocolVectors } from "../helpers/vectors.js";

const vectors = protocolVectors();

test("wallet.privateKeyImports: key -> pubkey -> address, byte-exact", () => {
  assert.ok(vectors.wallet.privateKeyImports.length >= 3);
  for (const vector of vectors.wallet.privateKeyImports) {
    const account = accountFromPrivateKey(vector.privateKeyHex);
    assert.equal(account.address, vector.address);
    assert.equal(account.addressChecksummed, vector.addressChecksummed);
    assert.equal(
      bytesToHex(account.publicKey),
      vector.publicKeyUncompressedHex,
    );
    assert.equal(account.privateKeyHex, vector.privateKeyHex);
  }
});

test("private key import accepts a missing 0x prefix, emits one", () => {
  const vector = vectors.wallet.privateKeyImports[0]!;
  const bare = vector.privateKeyHex.slice(2);
  const account = accountFromPrivateKey(bare);
  assert.equal(account.address, vector.address);
  assert.equal(account.privateKeyHex, vector.privateKeyHex);
  assert.ok(account.privateKeyHex.startsWith("0x"));
});

test("malformed private keys are rejected", () => {
  const N = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";
  assert.throws(() => normalizePrivateKey("0x" + "00".repeat(32)), /zero/);
  assert.throws(() => normalizePrivateKey(N), /curve order/); // == n
  assert.throws(() => normalizePrivateKey("ff".repeat(32)), /curve order/); // > n
  assert.throws(() => normalizePrivateKey("abcd"), /64 hex/); // short
  assert.throws(() => normalizePrivateKey("0x" + "ab".repeat(33)), /64 hex/); // long
  assert.throws(() => normalizePrivateKey("g".repeat(64)), /64 hex/); // non-hex
  assert.throws(() => normalizePrivateKey(""), /64 hex/);
  // n - 1 is the largest valid key:
  const nMinus1 =
    "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140";
  assert.equal(privateKeyToHex(normalizePrivateKey(nMinus1)), "0x" + nMinus1);
});

test("wallet.bip39Seeds: PBKDF2 seed derivation", () => {
  for (const vector of vectors.wallet.bip39Seeds) {
    assert.equal(vector.passphrase, "", "protocol pins an empty passphrase");
    assert.equal(bytesToHex(mnemonicToSeed(vector.phrase)), vector.seedHex);
  }
});

test("wallet.accounts: mnemonic -> m/44'/60'/0'/0/{index}", () => {
  assert.ok(vectors.wallet.accounts.length >= 4);
  for (const vector of vectors.wallet.accounts) {
    const account = accountFromMnemonic(vector.phrase, vector.index);
    assert.equal(account.path, vector.path);
    assert.equal(account.privateKeyHex, vector.privateKeyHex);
    assert.equal(account.address, vector.address);
    assert.equal(account.addressChecksummed, vector.addressChecksummed);
  }
});

test("mnemonics are trimmed before parsing; invalid ones are rejected", () => {
  const vector = vectors.wallet.accounts[0]!;
  const padded = `  ${vector.phrase}\n`;
  assert.equal(
    accountFromMnemonic(padded, vector.index).address,
    vector.address,
  );
  assert.throws(() => mnemonicToSeed("abandon ".repeat(12).trim()), /mnemonic/); // bad checksum
  assert.throws(
    () => mnemonicToSeed("definitely not a wordlist phrase"),
    /mnemonic/,
  );
});

test("wallet.eip55: checksum casing", () => {
  assert.ok(vectors.wallet.eip55.length >= 6);
  for (const vector of vectors.wallet.eip55) {
    assert.equal(toChecksumAddress(vector.lower), vector.checksummed);
    // Idempotent on already-checksummed input:
    assert.equal(toChecksumAddress(vector.checksummed), vector.checksummed);
  }
  assert.throws(() => toChecksumAddress("0x1234"), /invalid address/);
  assert.throws(
    () => toChecksumAddress("5aaeb6053f3e94c9b9a09f33669435e7ef1beaed"),
    /invalid address/,
  );
});

test("public keys are uncompressed 65-byte SEC1", () => {
  const vector = vectors.wallet.privateKeyImports[0]!;
  const pub = publicKeyFromPrivate(normalizePrivateKey(vector.privateKeyHex));
  assert.equal(pub.length, 65);
  assert.equal(pub[0], 0x04);
});
