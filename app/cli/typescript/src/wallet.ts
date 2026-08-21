/**
 * Wallet key handling: private-key import, address derivation, EIP-55
 * checksums and BIP-39/BIP-44 mnemonic derivation (the MetaMask path
 * `m/44'/60'/0'/0/{index}`).
 *
 * Byte-exact against `app/core/tests/vectors/protocol-v1.json` → `wallet.*`.
 */

import * as secp from "@noble/secp256k1";
import { keccak_256 } from "@noble/hashes/sha3";
import { HDKey } from "@scure/bip32";
import { mnemonicToSeedSync, validateMnemonic } from "@scure/bip39";
import { wordlist } from "@scure/bip39/wordlists/english";
import { bytesToHex, hexToBytes, isHex } from "./hex.js";

/** secp256k1 group order. */
const CURVE_N = secp.CURVE.n;

/**
 * Parse a private key from hex (the `0x` prefix is optional on input).
 * Rejects wrong length, non-hex characters, zero and values >= the curve
 * order.
 */
export function normalizePrivateKey(input: string): Uint8Array {
  const stripped = input.startsWith("0x") || input.startsWith("0X") ? input.slice(2) : input;
  if (stripped.length !== 64 || !isHex(stripped)) {
    throw new Error("private key must be 64 hex characters (optionally 0x-prefixed)");
  }
  const bytes = hexToBytes(stripped);
  let scalar = 0n;
  for (const b of bytes) scalar = (scalar << 8n) | BigInt(b);
  if (scalar === 0n) throw new Error("private key must not be zero");
  if (scalar >= CURVE_N) throw new Error("private key must be below the curve order");
  return bytes;
}

/** `0x` + 64 lowercase hex — the emitted form of a private key. */
export function privateKeyToHex(priv: Uint8Array): string {
  return `0x${bytesToHex(priv)}`;
}

/** Uncompressed SEC1 public key, 65 bytes (`04 || X || Y`). */
export function publicKeyFromPrivate(priv: Uint8Array): Uint8Array {
  return secp.getPublicKey(priv, false);
}

/**
 * Ethereum address from an uncompressed public key: keccak256 of the 64-byte
 * X||Y (dropping the leading `0x04` SEC1 byte), last 20 bytes. Lowercase.
 */
export function addressFromPublicKey(publicKey: Uint8Array): string {
  if (publicKey.length !== 65 || publicKey[0] !== 0x04) {
    throw new Error("expected a 65-byte uncompressed public key");
  }
  const hash = keccak_256(publicKey.subarray(1));
  return `0x${bytesToHex(hash.subarray(12))}`;
}

export function addressFromPrivateKey(priv: Uint8Array): string {
  return addressFromPublicKey(publicKeyFromPrivate(priv));
}

/**
 * EIP-55 checksummed form (display only — the wire form is lowercase).
 * Uppercase hex letter *i* iff nibble *i* of `keccak256(lower40)` >= 8.
 */
export function toChecksumAddress(address: string): string {
  const lower = address.toLowerCase();
  if (!/^0x[0-9a-f]{40}$/.test(lower)) {
    throw new Error(`invalid address: ${JSON.stringify(address)}`);
  }
  const body = lower.slice(2);
  const hash = bytesToHex(keccak_256(new TextEncoder().encode(body)));
  let out = "0x";
  for (let i = 0; i < body.length; i++) {
    const c = body[i]!;
    out += parseInt(hash[i]!, 16) >= 8 ? c.toUpperCase() : c;
  }
  return out;
}

export function isAddress(s: string): boolean {
  return /^0x[0-9a-fA-F]{40}$/.test(s);
}

export interface DerivedAccount {
  privateKey: Uint8Array;
  privateKeyHex: string;
  publicKey: Uint8Array;
  address: string;
  addressChecksummed: string;
  path: string;
  index: number;
}

/** BIP-39 seed for a (trimmed) English mnemonic, empty passphrase. */
export function mnemonicToSeed(phrase: string): Uint8Array {
  const trimmed = phrase.trim();
  if (!validateMnemonic(trimmed, wordlist)) {
    throw new Error("invalid BIP-39 mnemonic");
  }
  return mnemonicToSeedSync(trimmed, "");
}

/** Derive account `m/44'/60'/0'/0/{index}` from an English mnemonic. */
export function accountFromMnemonic(phrase: string, index = 0): DerivedAccount {
  if (!Number.isInteger(index) || index < 0) {
    throw new Error("account index must be a non-negative integer");
  }
  const seed = mnemonicToSeed(phrase);
  const path = `m/44'/60'/0'/0/${index}`;
  const node = HDKey.fromMasterSeed(seed).derive(path);
  if (!node.privateKey) {
    throw new Error(`derivation produced no private key at ${path}`);
  }
  const privateKey = normalizePrivateKey(bytesToHex(node.privateKey));
  const publicKey = publicKeyFromPrivate(privateKey);
  const address = addressFromPublicKey(publicKey);
  return {
    privateKey,
    privateKeyHex: privateKeyToHex(privateKey),
    publicKey,
    address,
    addressChecksummed: toChecksumAddress(address),
    path,
    index,
  };
}

/** Import a raw private key (hex, `0x` optional). */
export function accountFromPrivateKey(privateKeyHex: string): Omit<DerivedAccount, "path" | "index"> {
  const privateKey = normalizePrivateKey(privateKeyHex);
  const publicKey = publicKeyFromPrivate(privateKey);
  const address = addressFromPublicKey(publicKey);
  return {
    privateKey,
    privateKeyHex: privateKeyToHex(privateKey),
    publicKey,
    address,
    addressChecksummed: toChecksumAddress(address),
  };
}
