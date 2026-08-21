/** Small wire-protocol helpers with vectors in `protocol-v1.json`. */

import { sha256 } from "@noble/hashes/sha2";
import { bytesToHex, utf8Bytes } from "./hex.js";

/**
 * `msgHash` for a plaintext message: lowercase hex SHA-256 (never keccak) of
 * the **trimmed** content — the server trims before storing.
 */
export function msgHashPlaintext(content: string): string {
  return bytesToHex(sha256(utf8Bytes(content.trim())));
}

/**
 * `msgHash` for an encrypted message: SHA-256 of the base64 ciphertext string
 * exactly as sent, padding included. (E2EE itself is out of scope for this
 * client, but the hash rule is part of the wire protocol.)
 */
export function msgHashCiphertext(ciphertextBase64: string): string {
  return bytesToHex(sha256(utf8Bytes(ciphertextBase64)));
}

/**
 * A username for first-time logins that clears the server's rules
 * (3–100 chars, no `<>{};"'` backslash/backtick/comma, no control chars).
 * Derived from the wallet address plus a random tail so parallel test
 * accounts do not collide.
 */
export function generatedUsername(address: string): string {
  const tail = address.toLowerCase().replace(/^0x/, "").slice(0, 8);
  const rand = Math.floor(Math.random() * 10000)
    .toString()
    .padStart(4, "0");
  return `ts_${tail}_${rand}`;
}
