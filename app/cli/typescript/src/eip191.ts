/**
 * EIP-191 `personal_sign`: digest, deterministic signing (RFC 6979, low-S,
 * `v = recid + 27`) and address recovery.
 *
 * Byte-exact against `protocol-v1.json` → `eip191[]`.
 */

import * as secp from "@noble/secp256k1";
import { hmac } from "@noble/hashes/hmac";
import { sha256 } from "@noble/hashes/sha2";
import { keccak_256 } from "@noble/hashes/sha3";
import { addressFromPublicKey } from "./wallet.js";
import { bytesToHex, concatBytes, hexToBytes, utf8Bytes } from "./hex.js";

// noble-secp256k1 v2 needs a synchronous HMAC-SHA256 wired in for RFC 6979.
secp.etc.hmacSha256Sync = (key, ...msgs) =>
  hmac(sha256, key, secp.etc.concatBytes(...msgs));

const HALF_N = secp.CURVE.n >> 1n;

/**
 * keccak256( 0x19 || "Ethereum Signed Message:\n" || decimal(utf8_byte_len) || utf8(msg) ).
 *
 * The length is the UTF-8 **byte** length, not the character count.
 */
export function eip191Digest(message: string): Uint8Array {
  const body = utf8Bytes(message);
  const prefix = utf8Bytes(`\x19Ethereum Signed Message:\n${body.length}`);
  return keccak_256(concatBytes(prefix, body));
}

/**
 * Sign a message with EIP-191 personal_sign. Deterministic (RFC 6979, no
 * extra entropy), low-S normalized, `v = recovery_id + 27`.
 *
 * @returns `0x` + 130 lowercase hex (`r(32) || s(32) || v(1)`).
 */
export function personalSign(message: string, privateKey: Uint8Array): string {
  const digest = eip191Digest(message);
  const sig = secp.sign(digest, privateKey, {
    lowS: true,
    extraEntropy: false,
  });
  if (sig.recovery !== 0 && sig.recovery !== 1) {
    throw new Error(`unexpected recovery id ${sig.recovery}`);
  }
  const v = sig.recovery + 27;
  return `0x${bytesToHex(sig.toCompactRawBytes())}${v.toString(16).padStart(2, "0")}`;
}

export interface ParsedSignature {
  r: bigint;
  s: bigint;
  /** 0 or 1. */
  recovery: number;
}

/**
 * Parse a wire signature: `0x` + 130 hex. Emits 27/28 for `v` but tolerates
 * 0/1 on input. Rejects malformed hex, out-of-range `r`/`s` and — because
 * they are malleable — high-S signatures.
 */
export function parseSignature(signature: string): ParsedSignature {
  if (!/^0x[0-9a-fA-F]{130}$/.test(signature)) {
    throw new Error("signature must be 0x + 130 hex characters");
  }
  const bytes = hexToBytes(signature.slice(2));
  const r = secp.etc.bytesToNumberBE(bytes.subarray(0, 32));
  const s = secp.etc.bytesToNumberBE(bytes.subarray(32, 64));
  const v = bytes[64]!;
  let recovery: number;
  if (v === 27 || v === 28) recovery = v - 27;
  else if (v === 0 || v === 1) recovery = v;
  else
    throw new Error(
      `invalid recovery byte ${v} (expected 27/28, tolerating 0/1)`,
    );
  if (r <= 0n || r >= secp.CURVE.n) throw new Error("signature r out of range");
  if (s <= 0n || s >= secp.CURVE.n) throw new Error("signature s out of range");
  if (s > HALF_N) throw new Error("high-S signature rejected (malleable)");
  return { r, s, recovery };
}

/** Recover the lowercase signer address of an EIP-191 signature. */
export function recoverAddress(message: string, signature: string): string {
  const { r, s, recovery } = parseSignature(signature);
  const digest = eip191Digest(message);
  const publicKey = new secp.Signature(r, s)
    .addRecoveryBit(recovery)
    .recoverPublicKey(digest)
    .toBytes(false);
  return addressFromPublicKey(publicKey);
}

/** Verify that `signature` over `message` was made by `expectedAddress`. */
export function verifyPersonalSign(
  message: string,
  signature: string,
  expectedAddress: string,
): boolean {
  try {
    return recoverAddress(message, signature) === expectedAddress.toLowerCase();
  } catch {
    return false;
  }
}
