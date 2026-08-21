/** Minimal HS256 JWT minting for tampering tests (the server pins HS256). */

import { createHmac } from "node:crypto";

function b64url(input: Buffer | string): string {
  return Buffer.from(input).toString("base64url");
}

export interface JwtClaims {
  walletAddress: string;
  iat?: number;
  exp?: number;
  [extra: string]: unknown;
}

/** Sign `{ alg: HS256, typ: JWT }` over the given claims. */
export function mintJwt(secret: string, claims: JwtClaims): string {
  const now = Math.floor(Date.now() / 1000);
  const payload: Record<string, unknown> = {
    iat: now,
    exp: now + 60 * 60,
    ...claims,
  };
  const header = b64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
  const body = b64url(JSON.stringify(payload));
  const signature = createHmac("sha256", secret)
    .update(`${header}.${body}`)
    .digest("base64url");
  return `${header}.${body}.${signature}`;
}

/** Flip the last character of the signature — a structurally valid, wrong JWT. */
export function tamperSignature(jwt: string): string {
  const last = jwt.slice(-1);
  const replacement = last === "A" ? "B" : "A";
  return jwt.slice(0, -1) + replacement;
}
