/** Loads the canonical protocol test vectors from `app/core/tests/vectors`. */

import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export interface Eip191Vector {
  name: string;
  message: string;
  messageUtf8Len: number;
  privateKeyHex: string;
  address: string;
  digestHex: string;
  signatureHex: string;
}

export interface ProtocolVectors {
  eip191: Eip191Vector[];
  wallet: {
    accounts: {
      address: string;
      addressChecksummed: string;
      index: number;
      path: string;
      phrase: string;
      privateKeyHex: string;
    }[];
    bip39Seeds: { passphrase: string; phrase: string; seedHex: string }[];
    eip55: { checksummed: string; lower: string }[];
    privateKeyImports: {
      address: string;
      addressChecksummed: string;
      privateKeyHex: string;
      publicKeyUncompressedHex: string;
    }[];
  };
  msgHash: {
    plaintext: { content: string; msgHashHex: string; trimmedTo?: string }[];
    encrypted: { ciphertextBase64: string; msgHashHex: string }[];
    delete: { msgHash: string };
  };
  templates: Record<string, string>;
  usernames: { address: string; username: string }[];
}

/** Package root = the directory holding package.json, found from this file. */
export function packageRoot(): string {
  let dir = dirname(fileURLToPath(import.meta.url));
  for (let i = 0; i < 10; i++) {
    if (existsSync(join(dir, "package.json"))) return dir;
    dir = dirname(dir);
  }
  throw new Error("could not locate package.json above " + import.meta.url);
}

/** `app/` directory of the repository (package lives at `app/cli/typescript`). */
export function appRoot(): string {
  return join(packageRoot(), "..", "..");
}

let cached: ProtocolVectors | undefined;

export function protocolVectors(): ProtocolVectors {
  if (cached === undefined) {
    const path = join(
      appRoot(),
      "core",
      "tests",
      "vectors",
      "protocol-v1.json",
    );
    cached = JSON.parse(readFileSync(path, "utf8")) as ProtocolVectors;
  }
  return cached;
}
