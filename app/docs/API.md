# PocketSkynet HTTP API Specification

Implementation-ready spec for the FruitNation messenger wire protocol, reverse-engineered
from the authoritative Express implementation:

| Source | Role |
|---|---|
| `server/server/routes.ts` | **Source of truth** — every handler, check order, status code |
| `server/server/storage.ts` | Query semantics: ordering, filtering, joins, side effects |
| `server/server/security.ts` | Validation schemas + sanitizer |
| `server/shared/schema.ts` | Table/field definitions, insert schemas |
| `server/server/bootstrap.ts` | CORS, security headers, error handler, trust-proxy |
| `server/server/websocket.ts` | `/ws` notification channel |
| `server/PROTOCOL.md` | Prose docs (superseded by `routes.ts` where they disagree) |

**52 HTTP endpoints + 1 WebSocket endpoint.** Where the legacy implementation has a bug or
surprising behavior, it is called out inline as **[QUIRK]** with a recommendation. Reproduce
quirks only where wire compatibility with existing clients (Node/Python/Rust/Swift) requires it.

---

## 1. Transport Conventions

### 1.1 Base URL and ports

```
http://127.0.0.1:9081        # dev default (FN_SERVER_PORT, default 9081)
```

All API paths are prefixed `/api`. The WebSocket endpoint is `/ws` (not under `/api`).

### 1.2 Content type

- Requests with a body: `Content-Type: application/json`.
- Body size limit: **100 KB** (`express.json({ limit: "100kb" })`). Exceeding it produces
  `413` with `{"message": "..."}` from the generic error handler (`err.message` for 4xx).
- `application/x-www-form-urlencoded` is also parsed (`extended: false`) but no endpoint
  depends on it.
- All responses are `application/json`.

### 1.3 Authentication header

```
Authorization: Bearer <JWT>
```

**[QUIRK]** The middleware is `req.headers.authorization?.replace("Bearer ", "")` — a single
substring replace, not a prefix parse. Consequences to reproduce for compatibility:

- A bare token with **no** `Bearer ` prefix is accepted verbatim.
- `Bearer ` occurring anywhere in the header is stripped (first occurrence only).
- The scheme match is case-**sensitive**; `bearer <token>` fails JWT verification → 401.

Recommendation for the Rust port: accept both `Bearer <token>` (case-insensitive scheme) and
a bare token; do not emulate the mid-string replace.

Failures:

| Condition | Status | Body |
|---|---|---|
| Header absent / empty after strip | 401 | `{"message": "No token provided"}` |
| Signature/expiry/alg invalid | 401 | `{"message": "Invalid token"}` |

JWT verification pins `algorithms: ["HS256"]` (blocks `alg:none` substitution).

### 1.4 JSON serialization rules

Field names are **camelCase** (TypeScript property names), never the snake_case DB column names.

| DB type | JSON |
|---|---|
| `text` | string, or `null` when nullable and unset |
| `boolean` | `true` / `false` |
| `integer` / `serial` | number |
| `bigint` (`mode: "number"`) — `msgSerial`, `messageTimestamp`, `lastReadSerial` | **number** (JS-safe integer, not a string) |
| `timestamp with time zone` | ISO-8601 UTC string, e.g. `"2025-06-11T14:39:06.000Z"`; `null` when unset |

`undefined` values are **omitted** from the JSON object entirely (JS `JSON.stringify`
semantics). This matters for `lastMessage` and `sender` — see §5.

### 1.5 Error envelope

Every error body is an object with a `message` string. Three shapes exist:

```json
{ "message": "Access denied" }
```

```json
{ "message": "Validation failed", "errors": ["roomId: Room ID contains invalid characters"] }
```

The `errors` array is produced by `handleValidationError`: one entry per Zod issue, formatted
`` `${err.path.join(".")}: ${err.message}` ``. Always HTTP **400**.

```json
{ "code": "KEY_ROTATION_REQUIRED", "message": "…", "currentKeyVersion": 3 }
```

Only two endpoints emit a machine-readable `code`: `POST /api/rooms/:roomId/messages`
(`KEY_ROTATION_REQUIRED`, `STALE_KEY_VERSION`). Everything else is message-string only.

Unhandled errors reach the final handler: status ≥ 500 always responds
`{"message": "Internal Server Error"}` (internal detail is never leaked); status < 500 responds
`{"message": err.message || "Request failed"}`.

### 1.6 Response headers

Always set (`securityHeaders`):

```
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
Referrer-Policy: strict-origin-when-cross-origin
X-DNS-Prefetch-Control: off
Permissions-Policy: camera=(), microphone=(), geolocation=()
```

Production only (`NODE_ENV=production`):

```
Strict-Transport-Security: max-age=31536000; includeSubDomains
Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline';
  img-src 'self' data: https:; media-src https:;
  frame-src https://www.youtube.com https://www.youtube-nocookie.com;
  connect-src *; object-src 'none'; base-uri 'self'
```

Endpoint-specific: `GET /api/rooms/:roomId/sync` sets `X-Has-More: true|false`.

### 1.7 CORS (applies to `/api` only)

Allowlist (never a wildcard, because credentials are allowed):

- `http://localhost:5173`, `http://localhost:9081`, `http://127.0.0.1:5173`, `http://127.0.0.1:9081`
- `tauri://localhost`, `https://tauri.localhost`
- `process.env.CLIENT_URL`
- Any origin matching `/^https?:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/`

```
Access-Control-Allow-Origin: <echoed origin, only if allowlisted>
Access-Control-Allow-Methods: GET, POST, PUT, DELETE, PATCH, OPTIONS
Access-Control-Allow-Headers: Origin, X-Requested-With, Content-Type, Accept, Authorization
Access-Control-Allow-Credentials: true
Access-Control-Expose-Headers: X-Has-More
```

`OPTIONS` short-circuits with **200** and an empty body.

### 1.8 Client IP / trust proxy

`trust proxy` is **off** by default so `X-Forwarded-For` cannot be spoofed to defeat rate
limiting. `FN_TRUST_PROXY` overrides: `"true"`/`"false"` → boolean, digits → hop count, any
other string → passed through (`"loopback"`, a CIDR, …).

### 1.9 Required server environment

| Var | Required | Effect |
|---|---|---|
| `DATABASE_URL` | yes | Postgres connection |
| `JWT_SECRET` | **yes — fatal if unset** | HS256 signing key |
| `VITE_FRUITNATION_WALLET` | **yes — fatal if unset** | Server wallet for on-chain publish; returned in login + blockchain info |
| `FN_RPC_URL` | no | When set, `POST /api/messages/:id/publish` verifies the tx on-chain; when unset, format checks only (warn logged once) |
| `FN_DISABLE_RATE_LIMIT=1` | no | Disables all three limiters. **Fatal if `NODE_ENV=production`.** |
| `FN_TRUST_PROXY` | no | See §1.8 |
| `FN_SERVER_PORT` | no | Default 9081; invalid/out-of-range falls back to 9081 |
| `CLIENT_URL` | no | Extra CORS origin |
| `VITE_CHAIN_ID` | no | **Selects the chain the whole deployment runs on** — `25` (Cronos mainnet, the default when unset) or `338` (Cronos testnet). Drives both `GET /api/blockchain/info` and `GET /api/networks`, which serves exactly this one network: the chain is not a client-side preference, so the wallet cannot be pointed at a chain the server does not anchor to. An unknown or unparseable value falls back to mainnet rather than leaving the wallet without an RPC. |
| `VITE_CHAIN_RPC`, `VITE_CHAIN_NAME`, `VITE_CHAIN_EXPLORER`, `VITE_FRUITNATION_HASH_CRO` | no | Echoed by `GET /api/blockchain/info`. Each defaults to the value carried by the chain `VITE_CHAIN_ID` selected, so a deployment only sets these to override one detail — its own RPC endpoint, say — without leaving the chain. |

---

## 2. Rate Limiting

Three `express-rate-limit` instances, all keyed on **client IP**, all with a 60 000 ms fixed
window. They are **cumulative** — a login request consumes a slot in both the login limiter
and the general limiter.

| Limiter | Scope | Max / minute / IP | 429 body |
|---|---|---|---|
| `apiRateLimiter` | `app.use("/api", …)` — every `/api` route **registered after it** | 100 | `{"message": "Too many requests, please try again later"}` |
| `authChallengeRateLimiter` | `POST /api/auth/challenge` | 10 | `{"message": "Too many challenge requests, please try again later"}` |
| `authLoginRateLimiter` | `POST /api/auth/login` | 5 | `{"message": "Too many login attempts, please try again later"}` |

- **`GET /api/health` is exempt** — it is registered *before* `app.use("/api", apiRateLimiter)`
  so probes are never throttled. Every other `/api` route is subject to the 100/min limit.
- `standardHeaders: true`, `legacyHeaders: false` → responses carry `RateLimit-Limit`,
  `RateLimit-Remaining`, `RateLimit-Reset`; **no** `X-RateLimit-*` headers.
- Status on trip: **429**.
- The dev server enforces these too. Test harnesses must implement 429 backoff.

---

## 3. Input Validation Reference (`security.ts`)

Every named schema below is referenced by endpoint definitions in §6.

### 3.1 Scalar schemas

| Name | Rule |
|---|---|
| `walletAddress` | `/^0x[a-fA-F0-9]{40}$/`, then **lowercased**. Error: `"Invalid wallet address format"` |
| `username` | len 3–100; must NOT match `` /[<>{};"'`\\,]/ ``; must NOT contain control chars `/[\x00-\x1f\x7f]/`; must NOT match `/;\s*(DROP\|DELETE\|UPDATE\|INSERT\|CREATE\|ALTER\|EXEC\|UNION\|SELECT)\s+/i`; then `.trim()`. Unicode (Korean/CJK/emoji) allowed |
| `roomId` | len 10–100, `/^[a-zA-Z0-9_.-]+$/`, plus explicit reject of `' " ; $ % ( ) < >` (already excluded by the regex — redundant belt) |
| `messageId` | len 10–100, `/^[a-zA-Z0-9_-]+$/` (**no dot**, unlike roomId), same explicit rejects |
| `roomName` | len 1–100; must NOT match ``/[<>{};"'`\\]/``; then `.trim()`. Unicode allowed |
| `roomDescription` | max 500; same forbidden-char rule; `.trim()`; **optional** |
| `messageContent` | len 1–5000, then `.trim()`. **No content blocklist** — deliberately removed (bypassable + false positives on ciphertext) |
| `messageHash` | `/^[a-f0-9]{64}$/` — **lowercase hex only**. SHA-256 of the (cipher)text |
| `encryptionIV` (message) | `/^[a-f0-9]{32}$/`, `.nullish()` (may be `null` or absent) — lowercase only |
| `hmac` (message) | `/^[a-f0-9]{64}$/`, `.nullish()` — lowercase only |
| `encVer` | int 1–2, optional. 1 = legacy, 2 = key-separated KDF + authenticated IV |
| `keyVersion` | int 1–1 000 000, optional |
| `searchQuery` | len 1–100; must NOT match ``/[<>{};"'`\\]/``; `.trim()` |
| `emoticonCode` | len 1–64, `.trim()`. Any Unicode. Errors: `"Emoticon code is required"` / `"Emoticon code too long"` |
| `publicKeysAddresses` | array of `/^0x[a-fA-F0-9]{40}$/`, len 1–50. **Not** lowercased by the schema (lowercased at lookup time) |
| `limit` | int 1–100, default 50 |
| `serial` | int 0 … `Number.MAX_SAFE_INTEGER` (9007199254740991) |

**[QUIRK]** Zod runs `.min()/.max()` **before** `.transform(trim)`. A `content` of `"   "`
passes `min(1)` and is then stored as the empty string `""`. A Rust port using
`trim().is_empty()` rejection would diverge. Same applies to `roomName`, `username`.

### 3.2 `roomKeyFields` (object, spread into two endpoints)

```jsonc
{
  "encryptedSymmetricKey": "string, len 1..1024",           // base64 or hex, not format-checked
  "ephemeralPublicKey":    "string, len 1..256, /^[a-fA-F0-9]+$/",  // hex, MIXED case allowed
  "encryptionIV":          "string, /^[a-fA-F0-9]{32}$/",           // MIXED case allowed
  "hmac":                  "string, /^[a-fA-F0-9]{64}$/",           // MIXED case allowed
  "encVer":                "int 1..2, optional",
  "keyVersion":            "int 1..1000000, optional"
}
```

Note the case asymmetry: **room-key** hex fields accept mixed case (`a-fA-F`), **message**
`iv`/`hmac`/`msgHash` accept lowercase only (`a-f`).

### 3.3 Query schemas

| Schema | Fields |
|---|---|
| `pagination` (used by `GET /rooms/:id/messages`) | `since?`, `before?`: string → `parseInt(v,10)`; `NaN`/`<0`/`>MAX_SAFE_INTEGER` → `0`. `limit?`: string → `parseInt`; absent/`NaN`/`<1` → `50`; else `min(n, 100)` |
| `search` | `q`: `searchQuery` (**required**) |
| `messageSync` (used by `/sync`) | `since?`: string, default `"0"`; `parseInt`; `NaN`/`<0`/`>MAX_SAFE_INTEGER` → `0` |

**Important:** in `getMessages`, the filters are applied as `if (since)` / `if (before)`, so a
parsed value of **0 means "no filter"**, not "since epoch". A garbage `?since=abc` silently
becomes "no filter" rather than a 400.

### 3.4 `InputSanitizer`

- `validateParams(params, schema)` — URL-decodes each string param with `decodeURIComponent`
  (falling back to the raw value on `URIError`), then parses with the schema.
  **[QUIRK]** Express has *already* decoded `req.params`, so this is a **double decode**. A
  room/message ID is safe (`[A-Za-z0-9_.-]`), but see the emoticon-delete endpoint (§6.11.2).
- `sanitizeForSQL(input)` — strips `[\x00\x08\x09\x1a"'\\%]` and
  `/;\s*(DROP|DELETE|…)\s+/gi`, then trims. Applied **only to identifier-style params**
  (`roomId`, `messageId`, `address` in one place). Deliberately **not** applied to stored user
  content (message content, room name/description, username) — Drizzle is fully parameterized,
  and sanitizing mangled legitimate quotes/`%`. A Rust/axum port with `sqlx` bind parameters
  can drop `sanitizeForSQL` entirely; it is a no-op on all validated identifiers.
- `htmlEncode` — defined but **never called by any route**. XSS is handled at render time.

---

## 4. Database Model (for reference)

Ten tables. **No foreign keys and no `ON DELETE` cascades exist in the migrations** — all
referential integrity is enforced in application code.

```
users(wallet_address PK, username NOT NULL, public_key, public_key_sig, created_at, updated_at)
encryption_salts(wallet_address PK, salt NOT NULL, created_at)
auth_challenges(id PK, wallet_address, nonce, message, expires_at, created_at)
rooms(id PK, name, description, current_key_version NOT NULL DEFAULT 1,
      key_rotation_pending NOT NULL DEFAULT false, created_at)
room_admins(id serial PK, room_id, wallet_address, created_at)
room_members(id serial PK, room_id, user_address, joined_at)
room_invitations(id serial PK, room_id, invited_address, invited_by, created_at)
      UNIQUE(room_id, invited_address)
room_keys(id serial PK, room_id, user_address, encrypted_symmetric_key, ephemeral_public_key,
      encryption_iv, hmac, enc_ver NOT NULL DEFAULT 1, key_version NOT NULL DEFAULT 1, created_at)
blocked_users(id serial PK, blocker_address, blocked_address, created_at)
hidden_rooms(id serial PK, user_address, room_id, created_at)
messages(id PK, room_id, sender_address, content, msg_hash, message_timestamp bigint,
      msg_type NOT NULL DEFAULT 'message', msg_serial bigint NOT NULL DEFAULT 0,
      is_deleted NOT NULL DEFAULT false, edited_at, created_at,
      is_encrypted NOT NULL DEFAULT false, iv, hmac,
      enc_ver NOT NULL DEFAULT 1, key_version NOT NULL DEFAULT 1,
      tx_hash, target_message_id, emoticon_code)
      INDEX(room_id, msg_serial)
room_reads(id serial PK, room_id, user_address, last_read_serial bigint NOT NULL DEFAULT 0, updated_at)
      UNIQUE(room_id, user_address)
```

**[QUIRK] Missing unique indexes.** `blocked_users`, `hidden_rooms`, `room_members`, and
`room_admins` have **no** unique constraint, yet `blockUser` and `hideRoom` call
`.onConflictDoNothing()` with no conflict target. With no constraint to violate, that clause
is a no-op and **duplicate rows are inserted** on repeated `POST /api/users/block` or
`POST /api/rooms/:id/hide`. `GET /api/users/blocked` then returns the same address twice.
**Recommendation for PocketSkynet:** add
`UNIQUE(blocker_address, blocked_address)`, `UNIQUE(user_address, room_id)`,
`UNIQUE(room_id, user_address)`, `UNIQUE(room_id, wallet_address)` and make the inserts truly
idempotent. Clients tolerate deduplicated lists.

### 4.1 ID generation

| Entity | Format |
|---|---|
| Room | `room_{Date.now()}_{uuidv4()}` — e.g. `room_1749652739650_304e0eaf-bcf9-4682-a6a0-69bee8e40b97` |
| Message | `msg_{Date.now()}_{uuidv4()}` |
| Emoticon event | `emoticon_{Date.now()}_{uuidv4()}` |
| Auth challenge | `uuidv4()` (bare UUID) |

Room IDs contain `-` and `_` → valid under the `roomId` regex. Message IDs from emoticon
events likewise. **Note:** `messageId` forbids `.`, and no generated ID contains one.

---

## 5. Core Object Shapes

Exact serialized field names. Reproduce these verbatim.

### 5.1 `User`

```json
{
  "walletAddress": "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22",
  "username": "alice",
  "publicKey": "04f35792…9e",
  "publicKeySig": "0xe98d83…1b",
  "profileImage": "preset:tp-coder-f",
  "createdAt": "2025-06-11T14:39:06.000Z",
  "updatedAt": "2025-06-11T14:39:06.000Z"
}
```

- `walletAddress` is always **lowercase** (normalized on write by `securitySchemas.walletAddress`).
- `publicKey` — uncompressed secp256k1, 130 hex chars, **no** `0x` prefix. `null` if never published.
- `publicKeySig` — EIP-191 signature, `0x`-prefixed. `null` if not bound.
- `profileImage` — the chosen avatar (a PocketSkynet extension; the reference has no such
  field). Either `preset:<slug>` naming a portrait from the client's built-in gallery, or an
  `/api/images/<sha256>.<ext>` URL hosted by this server. `null` means clients derive the
  avatar from the address hash as before.
- `encryptionSalt` is **never** part of a `User` object (separate table, owner-only endpoints).

**Synthesized sender fallback.** When a message's sender has no `users` row, the join returns
`null` and the server substitutes:

```json
{
  "walletAddress": "0xabc…",
  "username": "User 0xabcd...ef01",
  "publicKey": null,
  "publicKeySig": null,
  "createdAt": "<now>",
  "updatedAt": "<now>"
}
```

Username format: `` `User ${addr.slice(0,6)}...${addr.slice(-4)}` `` (three literal dots).
**[QUIRK]** The fallback built in `getMessages` (full load) **omits `publicKeySig`**, while the
one in `getMessagesSinceSerial` (`/sync`) includes `publicKeySig: null`. Emit
`publicKeySig: null` in both; no client depends on the omission.

### 5.2 `Room`

```json
{
  "id": "room_1749652739650_304e0eaf-…",
  "name": "Team chat",
  "description": "Optional text or null",
  "currentKeyVersion": 1,
  "keyRotationPending": false,
  "createdAt": "2025-06-11T14:38:59.000Z"
}
```

### 5.3 `RoomWithMembers`

`Room` plus:

```json
{
  "memberCount": 3,
  "members": [ RoomMemberWithUser, … ],
  "admins": [ User, … ],
  "lastMessage": MessageWithSender,
  "hasEncryption": true,
  "unreadCount": 4,
  "lastReadSerial": 1749652746620
}
```

- `memberCount` = `members.length` (members whose `users` row exists — orphan members are
  dropped from `members` **and** from the count).
- `admins` — always present; `User` rows for `room_admins` entries whose user exists.
- `hasEncryption` — `true` iff **any** `room_keys` row exists for the room (any user, any epoch).
- `lastMessage` — the newest row satisfying `is_deleted = false`, scanned from the **10 most
  recent by `message_timestamp` DESC**, taking the first whose `msgType` is neither
  `emoticon_add` nor `emoticon_remove`. **Key is absent** (not `null`) when: no such row exists,
  more than 10 consecutive newest rows are emoticon events, or the sender has no `users` row.
  **[QUIRK]** `delete_all` markers are *not* excluded here (unlike in `getMessages`), so a
  `lastMessage` with `msgType: "delete_all"`, `content: ""` can surface. Filter it client-side
  or, for PocketSkynet, exclude `delete_all` in the query.
- `unreadCount` / `lastReadSerial` — **only** on `GET /api/rooms`. Absent from
  `GET /api/rooms/:roomId` and from the nested `room` in `GET /api/rooms/hidden`.

### 5.4 `RoomMemberWithUser`

```json
{
  "id": 42,
  "roomId": "room_…",
  "userAddress": "0x…",
  "joinedAt": "2025-06-11T14:39:00.000Z",
  "user": User
}
```

### 5.5 `Message` / `MessageWithSender`

```json
{
  "id": "msg_1749652746620_4cfe1c4c-…",
  "roomId": "room_…",
  "senderAddress": "0x742d35cc…",
  "content": "Hello everyone!",
  "msgHash": "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
  "messageTimestamp": 1749652746620,
  "msgType": "add",
  "msgSerial": 1749652746620,
  "isDeleted": false,
  "editedAt": null,
  "createdAt": "2025-06-11T14:39:06.000Z",
  "isEncrypted": false,
  "iv": null,
  "hmac": null,
  "encVer": 1,
  "keyVersion": 1,
  "txHash": null,
  "targetMessageId": null,
  "emoticonCode": null,
  "sender": User
}
```

`MessageWithSender` = the above **with** `sender`. Endpoints returning a bare `Message` never
include `sender` — except `POST/PATCH /messages` and `/publish`, which attach it explicitly.
**[QUIRK]** In `PATCH /api/messages/:messageId` and `POST /api/messages/:id/publish`, `sender`
is attached only `if (sender)`; when the lookup returns nothing the key is simply absent
(cannot happen in practice — the caller is authenticated and therefore has a row).

`msgType` values: `"add"`, `"edit"`, `"delete"`, `"delete_all"`, `"emoticon_add"`,
`"emoticon_remove"`. The DB default is `"message"`, and `PROTOCOL.md` mentions `"message"`, but
**no code path ever writes `"message"`** — `createMessage` hard-codes `"add"`. Clients should
treat `"message"` and `"add"` identically for safety.

### 5.6 `RoomKey`

```json
{
  "id": 17,
  "roomId": "room_…",
  "userAddress": "0x…",
  "encryptedSymmetricKey": "base64-or-hex…",
  "ephemeralPublicKey": "04a1b2…",
  "encryptionIV": "1a2b3c4d5e6f7890abcdef1234567890",
  "hmac": "9f8e7d…",
  "encVer": 2,
  "keyVersion": 3,
  "createdAt": "2025-06-11T14:39:06.000Z"
}
```

### 5.7 `BlockedUser` / `HiddenRoom`

```json
{ "id": 5, "blockerAddress": "0x…", "blockedAddress": "0x…", "createdAt": "2025-…" }
{ "id": 9, "userAddress": "0x…", "roomId": "room_…", "createdAt": "2025-…" }
```

### 5.8 `EmoticonAggregation`

```json
{ "emoticonCode": "🍎", "count": 2, "users": [ User, User ] }
```

`count` is the size of the reactor set. **[QUIRK]** `users` omits reactors with no `users` row,
so `count` can exceed `users.length`. Trust `count`.

---

## 6. Endpoint Reference

Legend: **Auth** = JWT required. All auth-required endpoints return the §1.3 401 bodies when
the token is missing/invalid; that row is not repeated per endpoint.

### 6.1 System (2)

#### 6.1.1 `GET /api/health` — Auth: **no**, rate-limited: **no**

Registered before the `/api` limiter, so load-balancer probes are never throttled.

| Status | Body |
|---|---|
| 200 | `{"status": "ok", "uptime": 12345}` — `uptime` = `Math.floor(process.uptime())`, whole seconds since process start |
| 503 | `{"status": "unavailable"}` — the `SELECT 1` probe against Postgres threw |

Note the body key is `status`, not `message`.

#### 6.1.2 `GET /api/blockchain/info` — Auth: **no**

All values are strings read from the environment; missing vars become `""` (except
`fruitnationHashCro`, which defaults to `"1.2"`).

```json
{
  "chainId": "338",
  "chainRpc": "https://evm-t3.cronos.org",
  "chainName": "Cronos Testnet",
  "chainExplorer": "https://explorer.cronos.org/testnet",
  "fruitnationHashCro": "1.2",
  "fruitnationWallet": "0x…"
}
```

| Status | Body |
|---|---|
| 200 | as above |
| 500 | `{"message": "Failed to get blockchain information"}` (unreachable in practice) |

#### 6.1.3 `GET /api/networks` — Auth: **no** *(PocketSkynet extension)*

The multi-chain registry behind the wallet's active-network switcher. Compiled
into `pocketskynet-core` (`chain::builtin_networks()`) and served from here so
a deployment can later override it without a client release. The wire type is
the `Network` struct itself — server and client deserialize the same code, so
they cannot drift. The **first entry is the default** and is deliberately a
testnet.

```json
[
  {
    "id": "cronos-testnet",
    "kind": "evm",
    "name": "Cronos Testnet",
    "chainId": 338,
    "rpcUrl": "https://evm-t3.cronos.org",
    "explorerUrl": "https://explorer.cronos.org/testnet",
    "symbol": "TCRO",
    "decimals": 18,
    "testnet": true,
    "tokens": []
  },
  {
    "id": "cronos-mainnet",
    "kind": "evm",
    "chainId": 25,
    "symbol": "CRO",
    "tokens": [
      { "symbol": "USDC", "name": "USD Coin",
        "contract": "0xc21223249ca28397b4b6541dffaecc539bff0c59", "decimals": 6 }
    ]
  }
]
```

`kind` is the signing family (`evm` | `solana` | `cardano`); only `evm`
networks support sending today — non-EVM entries appear in the switcher with
send disabled. `chainId` is omitted (not null) for non-EVM entries.

#### 6.1.4 `POST /api/images` — Auth: **✓** *(PocketSkynet extension)*

Hosts raw image bytes (AI generations) and returns a same-origin URL. The
reference client calls this endpoint but its server never implemented it;
here it exists. Body is the raw bytes, `Content-Type` must be `image/png`,
`image/jpeg`, `image/webp` or `image/gif`; limit **5 MB** (this route lifts
the 100 KB API-wide body cap). Storage is content-addressed
(`sha256(bytes).ext`), so re-uploading is idempotent.

| Status | Body |
|---|---|
| 200 | `{"url": "/api/images/<sha256>.<ext>"}` |
| 400 | wrong content type or empty body |
| 401 | no token |
| 413 | over 5 MB |

#### 6.1.5 `GET /api/images/{name}` — Auth: **no** *(PocketSkynet extension)*

Serves a stored image with `Cache-Control: public, max-age=31536000,
immutable` (sound: the name *is* the content hash). Unauthenticated because
image URLs are pasted into rooms and loaded by `<img>` tags, which cannot
attach a bearer header — the unguessable hash name is the capability. `name`
must be exactly 64 hex chars plus a known extension; anything else is 404.

---

### 6.2 Authentication (7)

#### 6.2.1 `POST /api/auth/challenge` — Auth: **no** — limit 10/min/IP

Request:

```json
{ "walletAddress": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266" }
```

| Field | Rule |
|---|---|
| `walletAddress` | required, `min(1)`, `/^0x[a-fA-F0-9]{40}$/`, lowercased server-side |

Side effects, in order:

1. `DELETE FROM auth_challenges WHERE expires_at < now()` (opportunistic GC on every call).
2. `nonce = randomBytes(32).toString("hex")` (64 hex chars).
3. `challengeId = uuidv4()`.
4. `expiresAt = now + 10 min`.
5. Insert into `auth_challenges`.

Challenge message — **exact bytes, `\n` is LF**:

```
Welcome to FruitNation!\n
\n
Click to sign in and accept the FruitNation Terms of Service.\n
\n
This request will not trigger a blockchain transaction or cost any gas fees.\n
\n
Wallet address:\n
{lowercased_wallet_address}\n
\n
Nonce:\n
{64_hex_nonce}
```

As a single JS template (authoritative):

```
`Welcome to FruitNation!\n\nClick to sign in and accept the FruitNation Terms of Service.\n\nThis request will not trigger a blockchain transaction or cost any gas fees.\n\nWallet address:\n${walletAddress}\n\nNonce:\n${nonce}`
```

No trailing newline.

200:

```json
{
  "challengeId": "6f1e2c30-…",
  "message": "Welcome to FruitNation!\n\n…",
  "expiresAt": "2025-06-11T14:49:06.000Z"
}
```

`expiresAt` is `Date.toISOString()`.

| Status | Body |
|---|---|
| 400 | `{"message":"Validation failed","errors":["walletAddress: Invalid wallet address format"]}` |
| 429 | `{"message":"Too many challenge requests, please try again later"}` |
| 500 | `{"message":"Failed to generate challenge"}` |

#### 6.2.2 `POST /api/auth/login` — Auth: **no** — limit 5/min/IP

Request:

```json
{
  "walletAddress": "0xf39Fd6…92266",
  "username": "alice",
  "challengeId": "6f1e2c30-…",
  "signature": "0x…",
  "publicKey": "04f357…9e",
  "publicKeySig": "0xe98d…1b"
}
```

| Field | Rule |
|---|---|
| `walletAddress` | required, address regex, lowercased |
| `username` | `z.any().transform(v => String(v \|\| "").trim())` — **any JSON type accepted**; `null`/`undefined`/`0`/`false` → `""`. Validated against `securitySchemas.username` **only if non-empty** |
| `challengeId` | required string, `min(1)` |
| `signature` | required, `max(200)`, `/^0x[a-fA-F0-9]+$/` |
| `publicKey` | optional, `max(130)`, `/^[a-fA-F0-9]+$/` (no `0x`) |
| `publicKeySig` | optional, `max(200)`, `/^0x[a-fA-F0-9]+$/` |

Handler order (each step's failure code matters):

1. **Consume the challenge atomically**: `DELETE … RETURNING`. A failed login therefore *burns*
   the challenge — clients must request a new one on every retry. → 400 `Invalid or expired challenge`
2. `now > challenge.expiresAt` → 400 `Challenge has expired`
3. `challenge.walletAddress !== walletAddress` (case-insensitive) → 400 `Wallet address mismatch`
4. `ethers.verifyMessage(challenge.message, signature)` (EIP-191 personal_sign) must recover
   `walletAddress` → else 401 `Invalid signature`; a throw inside verify → 401 `Invalid signature format`
5. Username resolution:
   - provided non-empty → validate with `securitySchemas.username`; on failure →
     400 `{"message": "<first zod issue message>"}` (**not** the `Validation failed` envelope)
   - empty **and** an existing user row with a username → reuse the stored username
   - otherwise → 400 `Username is required for first-time login`
6. If **both** `publicKey` and `publicKeySig` present: verify the key binding (§7.3);
   mismatch or throw → 400 `Invalid public key binding signature`.
   If only one of the pair is present, no verification runs.
7. `upsertUser({ walletAddress, username, publicKey: publicKey || undefined,
   publicKeySig: publicKey ? (publicKeySig || null) : null })` — `INSERT … ON CONFLICT
   (wallet_address) DO UPDATE SET username, public_key, public_key_sig, updated_at`.
8. `getOrCreateEncryptionSalt(walletAddress)` — 32 random bytes hex, created on first login;
   concurrent first logins race-safely keep the winner's salt.
9. Sign JWT: `jwt.sign({ walletAddress: user.walletAddress }, JWT_SECRET, { expiresIn: "30d" })`.

**[QUIRK] `publicKeySig` is wiped on every login that omits `publicKey`.** Because
`publicKeySig` computes to `null` (an explicit value, not `undefined`), Drizzle emits it in the
`SET` clause, while `publicKey: undefined` is omitted from `SET`. Net effect: a plain login
leaves `public_key` intact but sets `public_key_sig = NULL`, silently un-binding the key until
the client re-publishes via `PUT /api/auth/encryption-key`. **Recommendation:** in PocketSkynet,
leave both columns untouched when `publicKey` is absent.

200:

```json
{
  "user": User,
  "token": "<JWT>",
  "fruitnationWallet": "0x…",
  "encryptionSalt": "<64 hex>"
}
```

`fruitnationWallet` is the server's `VITE_FRUITNATION_WALLET`, echoed verbatim (not lowercased).

| Status | Body |
|---|---|
| 400 | `Validation failed` envelope; or one of the plain messages listed in steps 1–3, 5, 6 |
| 401 | `{"message":"Invalid signature"}` / `{"message":"Invalid signature format"}` |
| 429 | `{"message":"Too many login attempts, please try again later"}` |
| 500 | `{"message":"Login failed"}` |

#### 6.2.3 `POST /api/auth/logout` — Auth: **no** (no middleware)

Stateless; no server state is touched. Always:

```json
{ "message": "Logged out successfully" }
```

Status 200 even with no token. Clients discard the JWT locally.

#### 6.2.4 `GET /api/auth/encryption-salt` — Auth: **yes**

Returns the caller's own derivation salt, creating it if absent. **Never** exposed for any
other account — a public salt would let a hostile dapp reconstruct the derivation message and
phish the signature that *is* the user's E2EE private key.

| Status | Body |
|---|---|
| 200 | `{"salt": "<64 hex>"}` |
| 500 | `{"message":"Failed to get encryption salt"}` |

#### 6.2.5 `PUT /api/auth/encryption-key` — Auth: **yes**

Publish or rotate the caller's encryption public key together with its wallet binding.

```json
{ "publicKey": "04f357…9e", "publicKeySig": "0xe98d…1b" }
```

| Field | Rule |
|---|---|
| `publicKey` | required, `max(130)`, `/^[a-fA-F0-9]+$/` — error `"Public key must be hex"` |
| `publicKeySig` | required, `max(200)`, `/^0x[a-fA-F0-9]+$/` — error `"Signature must be a hex string"` |

Verifies `ethers.verifyMessage(buildKeyBindingMessage(callerAddress, publicKey), publicKeySig)`
recovers the caller's address (§7.3). Writes `users.public_key`, `users.public_key_sig`,
`updated_at`.

| Status | Body |
|---|---|
| 200 | `{"walletAddress": "0x…", "publicKey": "04…"}` — **only these two fields** |
| 400 | `Validation failed` envelope, or `{"message":"Invalid public key binding signature"}` |
| 500 | `{"message":"Failed to update encryption key"}` (also the path taken if no `users` row exists — `updateUser` returns `undefined` and dereferencing throws) |

#### 6.2.6 `GET /api/auth/profile` — Auth: **yes**

| Status | Body |
|---|---|
| 200 | `User` (the caller) |
| 404 | `{"message":"User not found"}` |
| 500 | `{"message":"Failed to get profile"}` |

#### 6.2.7 `PUT /api/auth/profile` — Auth: **yes**

```json
{ "username": "new_name", "profileImage": "preset:tp-coder-f" }
```

`username` is required and validated by `securitySchemas.username`. `profileImage` is an
optional PocketSkynet extension, three-valued: **absent** leaves the stored avatar
untouched, **`""`** clears it back to the hash-derived default, and a **value** must be
either `preset:<slug>` (slug alphabet `[a-z0-9-]`, ≤64 chars) or an
`/api/images/<sha256>.<png|jpg|webp|gif>` URL this server hosts — anything else is a 400,
because the stored value is served to other users' clients as an image source.
`walletAddress`, `publicKey`, `publicKeySig` cannot be changed here.

| Status | Body |
|---|---|
| 200 | updated `User` |
| 400 | `Validation failed` envelope |
| 500 | `{"message":"Failed to update profile"}` |

---

### 6.3 Users (3)

#### 6.3.1 `GET /api/users/search?q=<query>` — Auth: **yes**

`q` is **required** (`searchQuery`: 1–100 chars, no ``<>{};"'`\``). Missing → 400
`Validation failed`.

Query semantics (`storage.searchUsers`):

- LIKE metacharacters in `q` are escaped: `q.replace(/[\\%_]/g, ch => "\\" + ch)`.
- `WHERE username ILIKE '%q%' OR wallet_address ILIKE '%q%'`.
- Post-filter: removes every user the caller has blocked **and** every user who has blocked the
  caller (bidirectional invisibility).

**[QUIRK]** No `LIMIT` and no ordering. `q="0x"` returns every user with an address (i.e. all of
them) in indeterminate order. **Recommendation:** cap at e.g. 50 and order by `username` in
PocketSkynet. The caller's own account is *not* excluded and will appear in its own results.

| Status | Body |
|---|---|
| 200 | `User[]` (may be `[]`) |
| 400 | `Validation failed` envelope |
| 500 | `{"message":"Search failed"}` |

#### 6.3.2 `GET /api/users/:address` — Auth: **yes**

**Route ordering is load-bearing.** This pattern is registered *after* `/api/users/search`,
`/api/users/blocked`, and `/api/users/blocked-by` so those literals are not swallowed as an
`:address`. `POST /api/users/public-keys` is registered later but is a different method, so it
does not conflict. An axum router must preserve equivalent precedence (register literal
segments before the parameterized one, or use a router that prefers static segments).

`:address` validated by `securitySchemas.walletAddress` (lowercased), then passed through
`sanitizeForSQL` (no-op on valid addresses).

| Status | Body |
|---|---|
| 200 | `User` |
| 400 | `Validation failed` envelope (`address: Invalid wallet address format`) |
| 404 | `{"message":"User not found"}` |
| 500 | `{"message":"Failed to get user"}` |

Any authenticated user can read any other user's profile, including blocked ones — the block
filter applies only to `search`, messages, and typing relays.

#### 6.3.3 `POST /api/users/public-keys` — Auth: **yes**

```json
{ "addresses": ["0xabc…", "0xdef…"] }
```

`addresses`: array of address-regex strings, **1–50 entries**. Lowercased at lookup time.

Response — one entry per address that **both** resolves to a user **and** has a non-null
`publicKey`. Unknown addresses and users without keys are silently dropped; order follows the
request order of surviving entries.

```json
[
  { "walletAddress": "0xabc…", "publicKey": "04…", "publicKeySig": "0x…" },
  { "walletAddress": "0xdef…", "publicKey": "04…", "publicKeySig": null }
]
```

Clients **MUST** verify `publicKeySig` against the binding message (§7.3) before wrapping a
room key to `publicKey`. A `null` `publicKeySig` means unverifiable — refuse to wrap.

| Status | Body |
|---|---|
| 200 | array as above (possibly `[]`) |
| 500 | `{"message":"Failed to get public keys"}` |

**[QUIRK]** This handler's `catch` has **no** `ZodError` branch, so a malformed `addresses`
array (wrong type, empty, >50, bad address) returns **500**, not 400. **Recommendation:** return
400 with the standard `Validation failed` envelope in PocketSkynet; no client relies on the 500.

---

### 6.4 Blocking (5)

Blocking is **unidirectional in storage** (`blocker_address → blocked_address`) but **applied
bidirectionally** at several read paths. See §11 for the complete semantics.

#### 6.4.1 `GET /api/users/blocked` — Auth: **yes**

Rows where `blocker_address = caller`. Returns `BlockedUser[]` (not `User[]` — you get
addresses, not profiles).

| Status | Body |
|---|---|
| 200 | `BlockedUser[]` |
| 500 | `{"message":"Failed to get blocked users"}` |

#### 6.4.2 `GET /api/users/blocked-by` — Auth: **yes**

Rows where `blocked_address = caller`. **This tells the caller exactly who has blocked them** —
by design, so native clients can apply the same bidirectional filtering the web client does.

| Status | Body |
|---|---|
| 200 | `BlockedUser[]` |
| 500 | `{"message":"Failed to get blockers"}` |

#### 6.4.3 `POST /api/users/block` — Auth: **yes**

```json
{ "address": "0x…" }
```

Checks in order:

1. `address` missing or not a string → 400 `Wallet address is required`
2. Fails address regex → 400 `Invalid wallet address format` (a plain message, **not** the
   `Validation failed` envelope — `safeParse` is used here)
3. `address === caller` → 400 `Cannot block yourself`
4. Target user row missing → 404 `User not found`

Side effects: insert into `blocked_users`; then `refreshUserBlocks(caller)` and
`refreshUserBlocks(target)` update in-memory WebSocket block sets for **both** parties so
typing relays honor the block immediately. (Both fire *after* the response is sent.)

| Status | Body |
|---|---|
| 200 | `BlockedUser` row |
| 400 / 404 | as above |
| 500 | `{"message":"Failed to block user"}` |

**[QUIRK]** See §4 — repeat calls insert duplicate rows.

#### 6.4.4 `DELETE /api/users/block/:address` — Auth: **yes**

| Status | Body |
|---|---|
| 200 | `{"message":"User unblocked successfully"}` |
| 400 | `{"message":"Invalid wallet address format"}` |
| 500 | `{"message":"Failed to unblock user"}` |

**Idempotent and permissive:** unblocking someone who was never blocked returns 200. No
existence check on the target. Fires `refreshUserBlocks` for both parties after responding.
Deletes **all** matching rows (so duplicates from §4 are cleared in one call).

#### 6.4.5 `GET /api/users/:address/is-blocked` — Auth: **yes**

Direction: "have **I** blocked `:address`?" It does **not** report whether `:address` blocked me
(use `/api/users/blocked-by` for that).

| Status | Body |
|---|---|
| 200 | `{"isBlocked": true}` |
| 400 | `{"message":"Invalid wallet address format"}` |
| 500 | `{"message":"Failed to check block status"}` |

---

### 6.5 Rooms (8)

#### 6.5.1 `POST /api/rooms` — Auth: **yes**

```json
{ "name": "Team chat", "description": "optional" }
```

| Field | Rule |
|---|---|
| `name` | required, `roomName` (1–100, no ``<>{};"'`\``, trimmed) |
| `description` | optional, `roomDescription` (≤500, same forbidden chars, trimmed). Empty string → coerced to `undefined` → stored as SQL `NULL` |

Side effects: insert `rooms` (`currentKeyVersion = 1`, `keyRotationPending = false`), insert the
creator into `room_admins`, insert the creator into `room_members`. **No** WebSocket event is
emitted (the creator's socket subscription set is not refreshed — the client must refetch
`GET /api/rooms` itself).

| Status | Body |
|---|---|
| 200 | bare `Room` (no `members`/`admins`/`memberCount`) |
| 400 | `Validation failed` envelope |
| 500 | `{"message":"Failed to create room"}` |

Note: rooms are **not** publicly discoverable. Membership only arrives via creation or an
accepted invitation.

#### 6.5.2 `GET /api/rooms` — Auth: **yes**

Every room the caller is a member of, **excluding hidden rooms**, each enriched with
`unreadCount` and `lastReadSerial`.

- Source order: `SELECT room_id FROM room_members WHERE user_address = caller` — no `ORDER BY`,
  so effectively insertion order by the serial PK. **Not** sorted by activity. Clients sort by
  `lastMessage.messageTimestamp` themselves.
- `lastReadSerial` = the caller's `room_reads.last_read_serial`, or `0` if no row.
- `unreadCount` = `COUNT(*)` over `messages` where `room_id = …` **AND** `msg_serial > lastReadSerial`
  **AND** `msg_type = 'add'` **AND** `is_deleted = false` **AND** `sender_address <> caller`.
  Edits, deletes, `delete_all`, and emoticon events never count as unread. Blocked senders **do**
  count (the unread query is not block-filtered) — a known inconsistency with `/sync`.

| Status | Body |
|---|---|
| 200 | `RoomWithMembers[]` (with `unreadCount` + `lastReadSerial`) |
| 500 | `{"message":"Failed to get rooms"}` |

Performance note for the port: the reference does N+1 queries per room (members, admins, keys,
last message, unread). Batch these in PocketSkynet — the wire shape is unaffected.

#### 6.5.3 `GET /api/rooms/:roomId` — Auth: **yes**

**Check order:** membership is verified *before* room existence, so requesting a nonexistent
room returns **403, not 404**. This is intentional (no room-existence oracle). Reproduce it.

| Status | Body |
|---|---|
| 200 | `RoomWithMembers` (**no** `unreadCount` / `lastReadSerial`) |
| 400 | `Validation failed` envelope (bad `roomId` shape) |
| 403 | `{"message":"Access denied"}` — not a member, **or** the room does not exist |
| 404 | `{"message":"Room not found"}` — only reachable if the room is deleted between the two queries |
| 500 | `{"message":"Failed to get room"}` |

#### 6.5.4 `PATCH /api/rooms/:roomId` — Auth: **yes, admin-only**

```json
{ "name": "New name" }
```

`name` is required (`roomName`). **`description` cannot be updated** — no endpoint exists for it.

Check order: room exists → 404; caller is admin → 403.

| Status | Body |
|---|---|
| 200 | updated bare `Room` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Only room admins can update the room"}` |
| 404 | `{"message":"Room not found"}` |
| 500 | `{"message":"Failed to update room"}` |

No WebSocket event is emitted on rename.

#### 6.5.5 `DELETE /api/rooms/:roomId` — Auth: **yes, admin-only**

Check order: room exists → 404; caller is admin → 403.

Deletion order in `storage.deleteRoom` (no transaction, no FKs):
`messages` → `room_members` → `room_admins` → `room_keys` → `room_reads` → `hidden_rooms` → `rooms`.

**[QUIRK]** `room_invitations` rows are **not** deleted, leaving orphans. They are hidden from
`GET /api/invitations` (which drops invitations whose room is gone) but accumulate forever, and
`POST /api/invitations/:roomId/accept` on such an orphan returns 404 `Room no longer exists`
while cleaning up that one row. **Recommendation:** delete `room_invitations` too, inside a
transaction.

| Status | Body |
|---|---|
| 200 | `{"message":"Room deleted successfully"}` |
| 403 | `{"message":"Only room admins can delete the room"}` |
| 404 | `{"message":"Room not found"}` |
| 500 | `{"message":"Failed to delete room"}` |

No WebSocket event is emitted — other members' clients discover the deletion on their next
`GET /api/rooms`, and their `/sync` calls start returning 403.

#### 6.5.6 `POST /api/rooms/:roomId/leave` — Auth: **yes**

Body: none.

Checks: room exists → 404. If the caller is an admin **and** is the only admin → 400. Otherwise,
if the caller is an admin, their admin row is removed first.

Side effects, in order:

1. `removeRoomAdmin` (only if the caller was an admin and `adminCount > 1`)
2. `deleteRoomKey(roomId, caller)` — removes the caller's wraps for **all** epochs
3. `removeRoomMember(roomId, caller)` — also deletes the caller's `room_reads` row **and** their
   `hidden_rooms` row for that room (so a former member cannot keep reading it via
   `GET /api/rooms/hidden`)
4. `setKeyRotationPending(roomId, true)` — the leaver may still hold the current key
5. Response sent
6. `refreshUserRooms(caller)` → recomputes the caller's socket subscriptions + emits
   `{"type":"rooms_updated"}` to all their sockets
7. `notifyRoom(roomId, {"type":"member_removed","roomId":…})` to remaining members

| Status | Body |
|---|---|
| 200 | `{"message":"Left room successfully"}` |
| 400 | `{"message":"Cannot leave room as the last admin. Transfer admin rights first or delete the room."}` |
| 404 | `{"message":"Room not found"}` |
| 500 | `{"message":"Failed to leave room"}` |

**[QUIRK] No membership check.** Any authenticated user can `POST /leave` on any room they know
the ID of and receive 200 — and, critically, **set `keyRotationPending = true` on that room**,
which blocks all encrypted messaging (§6.9.1 returns 409) until a member rotates. This is a
remote denial-of-service on any room whose ID leaks. **Recommendation for PocketSkynet: require
`is_room_member` and return 403 otherwise.** No legitimate client depends on the permissive
behavior.

#### 6.5.7 `POST /api/rooms/:roomId/kick` — Auth: **yes, admin-only**

```json
{ "userAddress": "0x…" }
```

Check order: **caller is admin → 403 (checked before the body is parsed)**; then body validation
→ 400 envelope; then self-kick → 400; then target membership → 404.

Side effects: remove target's admin row if present → `deleteRoomKey` (all epochs) →
`removeRoomMember` (also drops target's `room_reads` + `hidden_rooms`) →
`setKeyRotationPending(true)` → respond → `refreshUserRooms(target)` (drops their socket
subscription, emits `rooms_updated`) → `notifyRoom(member_removed)`.

| Status | Body |
|---|---|
| 200 | `{"message":"User removed from room","keyRotationPending":true}` |
| 400 | `Validation failed` envelope, or `{"message":"Cannot kick yourself. Use leave instead."}` |
| 403 | `{"message":"Only room admins can remove members"}` |
| 404 | `{"message":"User is not a member of this room"}` |
| 500 | `{"message":"Failed to remove member"}` |

An admin **can** kick another admin (their admin row is removed first). There is no
last-admin guard on kick — an admin could in principle kick every other admin, but cannot kick
themselves, so at least one admin always remains.

#### 6.5.8 `GET /api/rooms/:roomId/members` — Auth: **yes, member-only**

| Status | Body |
|---|---|
| 200 | `RoomMemberWithUser[]` — insertion order, members with no `users` row are omitted |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to get room members"}` |

Blocked users are **not** filtered from the roster.

---

### 6.6 Hidden Rooms (3)

Hiding is a per-user client-side-list feature: it removes the room from `GET /api/rooms` but
does **not** affect membership, message delivery, or any other endpoint.

#### 6.6.1 `GET /api/rooms/hidden` — Auth: **yes**

Registered **before** `/api/rooms/:roomId` so `hidden` is not parsed as a room ID.

For each `hidden_rooms` row, membership is **re-checked** (a former member who hid the room
must not keep reading the roster or last-message preview) and the room detail is fetched. Rows
failing either check are skipped.

```json
[
  {
    "id": 9,
    "userAddress": "0x…",
    "roomId": "room_…",
    "createdAt": "2025-…",
    "room": RoomWithMembers
  }
]
```

The nested `room` has **no** `unreadCount` / `lastReadSerial`.

| Status | Body |
|---|---|
| 200 | array as above |
| 500 | `{"message":"Failed to get hidden rooms"}` |

#### 6.6.2 `POST /api/rooms/:roomId/hide` — Auth: **yes, member-only**

Body: none.

| Status | Body |
|---|---|
| 200 | `HiddenRoom` row |
| 403 | `{"message":"You must be a member of the room to hide it"}` |
| 500 | `{"message":"Failed to hide room"}` |

**[QUIRK]** This handler's `catch` has **no** `ZodError` branch → an invalid `roomId` yields
**500**, not 400. Same for unhide. **Recommendation:** return the standard 400 envelope.

**[QUIRK]** Repeat calls insert duplicate rows (§4), which then appear as duplicate entries in
`GET /api/rooms/hidden`.

#### 6.6.3 `DELETE /api/rooms/:roomId/hide` — Auth: **yes**

**No membership check and no existence check** — always succeeds. Deletes all matching rows.

| Status | Body |
|---|---|
| 200 | `{"message":"Room unhidden successfully"}` |
| 500 | `{"message":"Failed to unhide room"}` (including invalid `roomId`) |

---

### 6.7 Invitations (4)

See §10 for the complete lifecycle.

#### 6.7.1 `POST /api/rooms/:roomId/invite` — Auth: **yes, admin-only**

```json
{ "userAddress": "0x…" }
```

Check order (each returns immediately):

1. Room exists → 404 `Room not found`
2. Caller is admin → 403 `Only room admins can invite users`
3. Target user exists → 404 `User not found`
4. Target already a member → 400 `User is already a member of this room`
5. **Invitee has blocked the inviter** → 403 `You cannot invite users who have blocked you`
6. **Inviter has blocked the invitee** → 403 `You cannot invite users you have blocked`

Side effects: `INSERT INTO room_invitations … ON CONFLICT (room_id, invited_address) DO NOTHING`
(genuinely idempotent — this table *does* have the unique index). Then, after responding,
`notifyUser(invitee, {"type":"invitation_received","roomId":…})`.

**No membership row is created.** The invitee must accept.

| Status | Body |
|---|---|
| 200 | `{"message":"Invitation sent","pending":true}` |
| 400 | `Validation failed` envelope, or `User is already a member of this room` |
| 403 | one of the two block messages, or `Only room admins can invite users` |
| 404 | `Room not found` / `User not found` |
| 500 | `{"message":"Failed to invite user"}` |

Re-inviting someone who already has a pending invitation returns 200 and is a no-op.

#### 6.7.2 `GET /api/invitations` — Auth: **yes**

The caller's pending invitations, ordered by `created_at` **DESC**, enriched.

```json
[
  {
    "roomId": "room_…",
    "roomName": "Team chat",
    "invitedBy": "0x…",
    "inviterUsername": "alice",
    "createdAt": "2025-06-11T14:39:06.000Z"
  }
]
```

- `inviterUsername` falls back to the inviter's raw address when their `users` row is missing.
- Invitations whose room no longer exists are dropped from the response (but not from the DB).

**[QUIRK]** The drop is implemented as `roomName !== "(deleted room)"` — a room genuinely named
`(deleted room)` would be filtered out of every invitee's list. **Recommendation:** filter on
`room === undefined` instead.

| Status | Body |
|---|---|
| 200 | array as above (possibly `[]`) |
| 500 | `{"message":"Failed to list invitations"}` |

#### 6.7.3 `POST /api/invitations/:roomId/accept` — Auth: **yes**

Body: none. Note the path is `/api/invitations/...`, not `/api/rooms/...`.

1. Pending invitation for (roomId, caller) must exist → 404 `No pending invitation for this room`
2. Room must still exist → delete the invitation, then 404 `Room no longer exists`
3. `addRoomMember({roomId, userAddress: caller})`
4. `deleteRoomInvitation(roomId, caller)`
5. Respond
6. `refreshUserRooms(caller)` → subscribes the caller's sockets to the room + emits `rooms_updated`
7. `notifyRoom(roomId, {"type":"member_removed","roomId":…})` — deliberately reusing the
   `member_removed` type as a generic "membership changed, refresh" signal. Clients must treat
   `member_removed` as *"roster changed"*, not literally *"someone left"*.

| Status | Body |
|---|---|
| 200 | `{"message":"Invitation accepted","roomId":"room_…"}` |
| 400 | `Validation failed` envelope |
| 404 | `No pending invitation for this room` / `Room no longer exists` |
| 500 | `{"message":"Failed to accept invitation"}` |

Any room key an admin pre-wrapped for the invitee (§6.8.1) remains and becomes readable now
that the caller is a member.

#### 6.7.4 `POST /api/invitations/:roomId/decline` — Auth: **yes**

1. Pending invitation must exist → 404 `No pending invitation for this room`
2. Delete the invitation
3. `deleteRoomKey(roomId, caller)` — discards any pre-wrapped key across **all** epochs

| Status | Body |
|---|---|
| 200 | `{"message":"Invitation declined"}` |
| 400 | `Validation failed` envelope |
| 404 | `{"message":"No pending invitation for this room"}` |
| 500 | `{"message":"Failed to decline invitation"}` |

No WebSocket event; the inviter is not notified of a decline.

---

### 6.8 Room Admins (3)

Constraints: **min 1, max 9** admins per room. The creator is admin #1.

#### 6.8.1 `POST /api/rooms/:roomId/admins` — Auth: **yes, admin-only**

```json
{ "walletAddress": "0x…" }
```

Check order:

1. `walletAddress` missing / non-string → 400 `Wallet address is required`
2. Fails address regex → 400 `Invalid wallet address format` (plain message, `safeParse`)
3. Room exists → 404 `Room not found`
4. Caller is admin → 403 `Only room admins can add new admins`
5. `adminCount >= 9` → 400 `Maximum admin count (9) reached`
6. Target user exists → 404 `User not found`
7. Target is a room member → 400 `User must be a member of the room to become an admin`
8. Target already admin → 400 `User is already an admin`

| Status | Body |
|---|---|
| 200 | `{"message":"Admin added successfully"}` — the created row is **not** returned |
| 400 / 403 / 404 | as above |
| 500 | `{"message":"Failed to add admin"}` |

No WebSocket event.

#### 6.8.2 `DELETE /api/rooms/:roomId/admins/:walletAddress` — Auth: **yes, admin-only**

Both path params validated together (`roomId` + `walletAddress`); failure → 400 `Validation
failed` envelope. `walletAddress` is lowercased.

Check order: room exists → 404; caller is admin → 403; `adminCount <= 1` → 400; target is an
admin → 400.

| Status | Body |
|---|---|
| 200 | `{"message":"Admin removed successfully"}` |
| 400 | `Validation failed`; `Cannot remove the last admin. Room must have at least one admin.`; `User is not an admin` |
| 403 | `{"message":"Only room admins can remove admins"}` |
| 404 | `{"message":"Room not found"}` |
| 500 | `{"message":"Failed to remove admin"}` |

An admin **may** demote themselves as long as another admin remains. Removing admin status does
**not** remove membership.

#### 6.8.3 `GET /api/rooms/:roomId/admins` — Auth: **yes, member-only**

| Status | Body |
|---|---|
| 200 | `User[]` — full profiles, insertion order; admins with no `users` row are omitted |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to get admins"}` |

---

### 6.9 Room Keys & Rotation (4)

See §9 for the epoch model.

#### 6.9.1 `POST /api/rooms/:roomId/keys` — Auth: **yes**

Store one wrapped room key for one user, for one epoch.

```json
{
  "userAddress": "0x…",
  "encryptedSymmetricKey": "…",
  "ephemeralPublicKey": "04…",
  "encryptionIV": "1a2b…(32 hex)",
  "hmac": "…(64 hex)",
  "encVer": 2,
  "keyVersion": 1
}
```

Validation: `userAddress` = `walletAddress` schema (lowercased), plus all of `roomKeyFields`
(§3.2). `encVer` defaults to **1**, `keyVersion` defaults to **1** when absent.

Authorization, in order:

1. Room exists → 404 `Room not found`
2. Target is a member **or** has a pending invitation → else 400 `User must be a room member or
   invitee`. (Pre-wrapping at invite time is the whole point: the admin is online then, the
   invitee may not be.)
3. Caller is storing **their own** key (`caller == userAddress`) **or** caller is a room admin →
   else 403 `Only admins can store keys for other users`
4. If storing for **someone else**: a wrap for `(room, target, keyVersion)` must not already
   exist → else **409** `That member already has a key for this epoch; use /rotate-key to re-key
   the room.` This prevents an admin from clobbering a member's valid wrap and locking them out.
   Members may always overwrite their **own** wrap for an epoch.

Write semantics (`addRoomKey`): `DELETE FROM room_keys WHERE room_id=? AND user_address=? AND
key_version=?` then `INSERT`. Only the targeted epoch is replaced — other epochs survive so the
member keeps access to history.

| Status | Body |
|---|---|
| 200 | `{"message":"Room key stored successfully"}` |
| 400 | `Validation failed` envelope, or `User must be a room member or invitee` |
| 403 | `{"message":"Only admins can store keys for other users"}` |
| 404 | `{"message":"Room not found"}` |
| 409 | `{"message":"That member already has a key for this epoch; use /rotate-key to re-key the room."}` |
| 500 | `{"message":"Failed to store room key"}` |

Note this endpoint does **not** touch `rooms.current_key_version`. Establishing epoch 1 is a
plain store; advancing an epoch requires `/rotate-key`.

#### 6.9.2 `GET /api/rooms/:roomId/keys` — Auth: **yes, member-only**

The caller's **latest** wrap: `ORDER BY key_version DESC, id DESC LIMIT 1`.

| Status | Body |
|---|---|
| 200 | a single `RoomKey` object |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 404 | `{"message":"Room key not found"}` — the room is unencrypted, or the caller has no wrap |
| 500 | `{"message":"Failed to get room key"}` |

#### 6.9.3 `GET /api/rooms/:roomId/keys/versions` — Auth: **yes, member-only**

**All** of the caller's wraps for the room, `ORDER BY key_version ASC` — one per epoch they can
access. The client unwraps each so it can decrypt messages from every epoch in its history.

| Status | Body |
|---|---|
| 200 | `RoomKey[]` (possibly `[]` — an empty array, **not** a 404) |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to get room key versions"}` |

#### 6.9.4 `POST /api/rooms/:roomId/rotate-key` — Auth: **yes, member-only** (deliberately *not* admin-only)

```json
{
  "newVersion": 2,
  "keys": [
    { "userAddress": "0x…", "encryptedSymmetricKey": "…", "ephemeralPublicKey": "04…",
      "encryptionIV": "…", "hmac": "…", "encVer": 2 }
  ]
}
```

| Field | Rule |
|---|---|
| `newVersion` | required int, **2 ≤ n ≤ 1 000 000** (1 is not a valid rotation target) |
| `keys` | required array, **1–200** entries, each `{ userAddress } ∪ roomKeyFields`. Per-entry `keyVersion` in the body is **ignored** — the server forces `keyVersion = newVersion`. Per-entry `encVer` defaults to **2** here (vs 1 in `/keys`) |

Any **current member** may rotate — Signal-style, the sender drives the re-key. Gating this to
admins would freeze a room after a departure until an admin appeared. A member already holds the
current key, so rotating reveals nothing new to them, and the two coverage checks below prevent
lock-outs and outsider injection.

Checks, in order:

1. Caller is a member → 403 `Only room members can rotate the room key`
2. Body parse → 400 `Validation failed` envelope
3. **Full coverage:** every current member must appear in `keys` (case-insensitive address
   compare) → else 400 with a `missing` array
4. **No strays:** no entry may target a non-member → else 400 `Rotation includes a non-member address`
5. Inside a **transaction** (`storage.rotateRoomKey`):
   - room must exist → `{ok:false, reason:"Room not found"}` → 409
   - `newVersion === room.current_key_version + 1` exactly → else
     `{ok:false, reason:"Stale key version — refetch and retry"}` → 409
   - for each wrap: delete any `(room, user, newVersion)` row, insert the new one
   - `UPDATE rooms SET current_key_version = newVersion, key_rotation_pending = false`

Side effects after responding: `notifyRoom(roomId, {"type":"new_message","roomId":…})` — clients
must treat `new_message` as "something changed; re-sync **and** refetch key versions".

| Status | Body |
|---|---|
| 200 | `{"message":"Room key rotated","newVersion":2}` |
| 400 | `Validation failed` envelope; `{"message":"Rotation must include a key for every current member","missing":["0x…","0x…"]}`; `{"message":"Rotation includes a non-member address"}` |
| 403 | `{"message":"Only room members can rotate the room key"}` |
| 409 | `{"message":"Stale key version — refetch and retry"}` or `{"message":"Room not found"}` or `{"message":"Rotation failed"}` |
| 500 | `{"message":"Failed to rotate room key"}` |

The 409 with `Stale key version` is the concurrency signal: two members racing to rotate — the
loser refetches `GET /api/rooms/:roomId` for the new `currentKeyVersion` and retries only if
`keyRotationPending` is still true.

---

### 6.10 Messages (6)

#### 6.10.1 `POST /api/rooms/:roomId/messages` — Auth: **yes, member-only**

```json
{
  "content": "ciphertext-or-plaintext",
  "msgHash": "<64 lowercase hex>",
  "isEncrypted": true,
  "iv": "<32 lowercase hex>",
  "hmac": "<64 lowercase hex>",
  "encVer": 2,
  "keyVersion": 3
}
```

| Field | Rule |
|---|---|
| `content` | **required**, 1–5000 chars (pre-trim), then trimmed. For encrypted messages this is the base64/hex ciphertext |
| `msgHash` | **required**, `/^[a-f0-9]{64}$/` — SHA-256 of the `content` as sent (i.e. of the ciphertext for encrypted messages) |
| `isEncrypted` | optional boolean, default `false` |
| `iv` | optional, `/^[a-f0-9]{32}$/` or `null` |
| `hmac` | optional, `/^[a-f0-9]{64}$/` or `null` |
| `encVer` | optional int 1–2, stored as `encVer ?? 1` |
| `keyVersion` | optional int 1–1 000 000, stored as `keyVersion ?? 1` |

Forward-secrecy gate — **only for `isEncrypted: true`**:

1. Room must exist → 404 `Room not found`
2. `room.keyRotationPending === true` → **409**
   ```json
   { "code": "KEY_ROTATION_REQUIRED",
     "message": "Room key rotation is pending — an admin must rotate the key before new encrypted messages can be sent.",
     "currentKeyVersion": 3 }
   ```
   A member left/was kicked and the key has not been re-keyed; refusing here means a removed
   member's cached key can never read anything sent after their departure.
3. `(keyVersion ?? 1) !== room.currentKeyVersion` → **409**
   ```json
   { "code": "STALE_KEY_VERSION",
     "message": "Message key version does not match the room's current epoch — refetch keys and retry.",
     "currentKeyVersion": 4 }
   ```

Unencrypted rooms/messages skip all three checks entirely — the room is not even fetched.

Server-controlled fields (client values are ignored or absent):

| Field | Value |
|---|---|
| `id` | `msg_{now}_{uuid4}` |
| `senderAddress` | from the JWT (**not** from the body) |
| `messageTimestamp` | `Date.now()` at insert — set twice (route + `createMessage`); the storage-layer value wins |
| `msgType` | hard-coded `"add"` |
| `msgSerial` | see §8.3 |
| `isDeleted` | `false` |
| `txHash`, `editedAt`, `targetMessageId`, `emoticonCode` | `null` |

Blocking note: **blocked users can still post.** The blocker simply never receives the message
(server-side filtering on read), so other room members still see it.

| Status | Body |
|---|---|
| 200 | `MessageWithSender` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` — not a member (or the room does not exist) |
| 404 | `{"message":"Room not found"}` — encrypted path only |
| 409 | `KEY_ROTATION_REQUIRED` / `STALE_KEY_VERSION` (see above) |
| 500 | `{"message":"Failed to send message"}` |

Side effect after responding: `notifyRoom(roomId, {"type":"new_message","roomId":…})`.

#### 6.10.2 `GET /api/rooms/:roomId/messages` — Auth: **yes, member-only**

Full/backfill load. Query params (`querySchemas.pagination`, §3.3):

| Param | Type | Default | Semantics |
|---|---|---|---|
| `since` | ms epoch string | none | `message_timestamp >= since`. A parsed `0` disables the filter |
| `before` | ms epoch string | none | `message_timestamp < before` (backward pagination). A parsed `0` disables the filter |
| `limit` | string | `50` | clamped to `[1, 100]`; garbage → `50` |

Query: `WHERE room_id = ? AND is_deleted = false [AND …] [AND sender_address NOT IN (blocked)]
ORDER BY message_timestamp DESC LIMIT ?`, LEFT JOIN `users`. Then, **in application code**,
rows with `msgType` in `{emoticon_add, emoticon_remove, delete_all}` are dropped, and the array
is `.reverse()`d → returned **chronologically ascending**.

**[QUIRK] The limit is applied before the msgType filter.** A page can therefore contain fewer
than `limit` messages even when older ones exist. Clients must paginate on the oldest returned
`messageTimestamp`, never on the returned count. **Recommendation:** filter in SQL.

Backward-pagination recipe: first call `?limit=50`; then `?before=<oldest messageTimestamp
returned>&limit=50`; repeat until empty.

`since`/`before` are **timestamps**, unlike `/sync`'s `since`, which is a **serial**. Do not mix.

| Status | Body |
|---|---|
| 200 | `MessageWithSender[]`, ascending by `messageTimestamp` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to get messages"}` |

#### 6.10.3 `PATCH /api/messages/:messageId` — Auth: **yes, owner-only**

Same body schema as `POST …/messages` (`content` and `msgHash` required; `isEncrypted` is
parsed but **ignored** — encryption is inferred from `iv`+`hmac`).

Checks: message exists **and is not already deleted** (`getMessageById` filters
`is_deleted = false`) → 404; caller is a member of the message's room → 403; caller is the
message's sender → 403.

Update (`storage.updateMessage`):

```
content    = <new>
msgHash    = <new>
editedAt   = now()
msgType    = "edit"
msgSerial  = nextSerial(roomId)
if (iv !== undefined && hmac !== undefined):
    iv, hmac = <new>;  isEncrypted = true;  encVer = encVer ?? 1;  keyVersion = keyVersion ?? 1
else:
    iv = null;  hmac = null;  isEncrypted = false;  encVer = 1;  keyVersion = 1
```

The row is updated **in place** — the message keeps its `id`, `createdAt`, and
`messageTimestamp`; only `msgSerial` advances so `/sync` re-delivers it.

**[QUIRK] Edits bypass the forward-secrecy gate.** Neither `keyRotationPending` nor
`currentKeyVersion` is checked on edit, so a client can write content under a stale epoch that
`POST` would have rejected with 409. **Recommendation:** apply the same two 409 checks in
PocketSkynet.

**[QUIRK]** Omitting `iv`/`hmac` on an edit **silently downgrades an encrypted message to
plaintext** (`isEncrypted = false`, `keyVersion = 1`). Encrypting clients must always resend
`iv` and `hmac`.

| Status | Body |
|---|---|
| 200 | `MessageWithSender` (updated row) |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Not a member of this room"}` / `{"message":"Only the message owner can edit this message"}` |
| 404 | `{"message":"Message not found"}` / `{"message":"Message not found or unauthorized"}` |
| 500 | `{"message":"Failed to update message"}` |

Side effect: `notifyRoom(roomId, {"type":"new_message","roomId":…})`.

#### 6.10.4 `DELETE /api/messages/:messageId` — Auth: **yes, any member of the room**

**Any room member can delete any message** — "forgetting-first" privacy, deliberate and not a bug.

Checks: message exists and not already deleted → 404; caller is a member of the message's room → 403.
There is **no** ownership check.

Update (soft delete, maximal scrub):

```
isDeleted = true;  content = "";  msgHash = "";  msgType = "delete";
msgSerial = nextSerial(roomId);  iv = null;  hmac = null
```

`senderAddress`, `messageTimestamp`, `encVer`, `keyVersion`, `txHash` are retained.

| Status | Body |
|---|---|
| 200 | `{"message":"Message deleted successfully"}` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Not a member of this room"}` |
| 404 | `{"message":"Message not found"}` / `{"message":"Message not found or unauthorized"}` |
| 500 | `{"message":"Failed to delete message"}` |

Side effect: `notifyRoom(roomId, {"type":"new_message","roomId":…})`.

#### 6.10.5 `DELETE /api/rooms/:roomId/messages` — Auth: **yes, any member**

Nuke the room's entire history. Any member may do this.

Side effects (`storage.deleteAllMessages`), not transactional:

1. `SELECT id FROM messages WHERE room_id = ?` → `deletedCount` = row count (**includes**
   emoticon events, already-deleted rows, and prior `delete_all` markers)
2. `DELETE FROM messages WHERE room_id = ?` — **hard** delete, rows are physically gone
3. Insert exactly one marker row: `msgType = "delete_all"`, `content = ""`, `msgHash = ""`,
   `senderAddress` = caller, `messageTimestamp = Date.now()`, `msgSerial = nextSerial(roomId)`,
   `isDeleted = false`, `isEncrypted = false`

The marker exists so `/sync` clients learn to clear their local caches. Note step 3 computes the
next serial *after* the table was emptied — see §8.3 for why the in-process counter keeps it
monotonic within a single server process, and why a Rust port must persist a per-room counter.

| Status | Body |
|---|---|
| 200 | `{"message":"All messages deleted successfully","deletedCount":137}` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Not a member of this room"}` |
| 500 | `{"message":"Failed to delete all messages"}` |

Side effect: `notifyRoom(roomId, {"type":"new_message","roomId":…})`.

#### 6.10.6 `POST /api/messages/:messageId/publish` — Auth: **yes, message-sender-only**

Record an on-chain transaction hash that anchors a message's `msgHash`.

```json
{ "txHash": "0x<64 hex>", "toAddress": "0x<40 hex>" }
```

Validation is **hand-rolled** here (no Zod): `/^0x[a-fA-F0-9]{64}$/` for `txHash`,
`/^0x[a-fA-F0-9]{40}$/` for `toAddress`.

Checks, in order:

1. `txHash` bad → 400 `Invalid transaction hash format`
2. `toAddress` bad → 400 `Invalid to address format`
3. `toAddress.toLowerCase() !== FRUITNATION_WALLET.toLowerCase()` → 400 `Publishing hash failed:
   transaction recipient does not match server wallet`
4. **If `FN_RPC_URL` is set** (otherwise skipped entirely):
   - message must exist (and not be deleted) → 400 `Message not found`
   - `provider.getTransaction(txHash)`; RPC throw → **502** `Failed to verify transaction on-chain`
   - `tx == null` → 400 `Transaction not found on-chain`
   - `tx.to` must equal the server wallet → 400 (same recipient-mismatch message)
   - `tx.data` (lowercased) must **contain** `msgHash.toLowerCase().replace(/^0x/,"")` → 400
     `Publishing hash failed: transaction data does not contain the message hash`
5. `storage.publishMessageTxHash`:
   - message must exist → throws `Message not found`
   - `senderAddress !== caller` → throws `Only the message sender can publish a transaction hash`
   - `txHash` already set → throws `Message already has a transaction hash`
   - else `UPDATE messages SET tx_hash = ?, msg_serial = nextSerial(roomId)` — the serial bump
     makes `/sync` redeliver the row so clients pick up the anchor

**[QUIRK] Every failure is 400**, including "not found" and authorization failures. The catch
block allowlists three business messages and maps anything else to
`Failed to publish transaction hash`. An invalid `messageId` path param also lands here as
**400 `Failed to publish transaction hash`** (its Zod error is not special-cased).

**[QUIRK] No room-membership check** — only sender ownership, enforced in the storage layer.

**[QUIRK]** In step 5, `publishMessageTxHash` selects **without** the `is_deleted = false`
filter, so a deleted message (whose `msgHash` is now `""`) can be published when `FN_RPC_URL`
is unset. With the RPC configured, step 4's `getMessageById` filters deleted rows first.

| Status | Body |
|---|---|
| 200 | `MessageWithSender` (updated row, `sender` attached) |
| 400 | any of the messages above, or `{"message":"Failed to publish transaction hash"}` |
| 502 | `{"message":"Failed to verify transaction on-chain"}` |

No WebSocket event is emitted.

---

### 6.11 Emoticons (3)

Reactions are **not** a separate table — each add/remove is an append-only row in `messages`
with `targetMessageId` + `emoticonCode` set, so reactions flow through `/sync` like everything else.

#### 6.11.1 `POST /api/messages/:messageId/emoticons` — Auth: **yes, member-only**

```json
{ "emoticonCode": "🍎" }
```

`emoticonCode`: 1–64 chars, trimmed, any Unicode.

Checks: target message exists and is not deleted → 404; caller is a member of the target's room → 403.

Creates a new `messages` row:

| Field | Value |
|---|---|
| `id` | `emoticon_{now}_{uuid4}` |
| `roomId` | the **target message's** room |
| `senderAddress` | caller |
| `content` | `""` |
| `msgType` | `"emoticon_add"` |
| `msgHash` | `sha256hex("{messageId}:{emoticonCode}:add:{callerAddress}:{timestamp}")` |
| `messageTimestamp` | `Date.now()` (same value used in the hash preimage) |
| `msgSerial` | §8.3 |
| `targetMessageId` | `:messageId` |
| `emoticonCode` | as supplied |
| `isEncrypted`, `isDeleted` | `false` (column defaults) |
| `iv`, `hmac`, `txHash`, `editedAt` | `null` |
| `encVer`, `keyVersion` | `1` (column defaults) |

The `msgHash` preimage is a plain UTF-8 string joined with `:` — reproduce it byte-for-byte.

| Status | Body |
|---|---|
| 200 | the created `Message` row (**no** `sender`) |
| 400 | `Validation failed` envelope, or `{"message":"You have already added this emoticon"}` |
| 403 | `{"message":"Access denied"}` |
| 404 | `{"message":"Message not found"}` |
| 500 | `{"message":"Failed to add emoticon"}` |

**[QUIRK]** The "already added" 400 is **dead code** — `createEmoticonEvent` never throws it and
no duplicate check exists. Duplicate adds simply append another event; aggregation is
set-based so the visible result is unchanged. Emit no such error in PocketSkynet, or add a real
duplicate check (behavior-compatible either way).

Reacting to your own message is allowed. Blocked users' reactions still land, but their events
are filtered out of the blocker's `/sync` (§11).

#### 6.11.2 `DELETE /api/messages/:messageId/emoticons/:emoticonCode` — Auth: **yes, member-only**

`:emoticonCode` must be **percent-encoded** in the URL (`🍎` → `%F0%9F%8D%8E`).

**[QUIRK] Double decoding.** Express already percent-decodes `req.params`, and the handler then
calls `decodeURIComponent(req.params.emoticonCode)` again. An emoticon code containing a literal
`%` therefore round-trips incorrectly (`%25` → `%` → decode error or corruption), and a code
containing an encoded `%2F` may be mangled. **Recommendation:** decode exactly once. The
`:messageId` param has the same double decode via `validateParams`, but is harmless because
valid IDs contain no `%`.

Creates an `emoticon_remove` row with the identical field layout as `emoticon_add`, except:

- `msgType = "emoticon_remove"`
- `msgHash = sha256hex("{messageId}:{emoticonCode}:remove:{callerAddress}:{timestamp}")` (note
  `remove`, not `add`)

Removing an emoticon you never added is allowed and appends a no-op event.

| Status | Body |
|---|---|
| 200 | `{"message":"Emoticon removed successfully"}` — the created row is **not** returned |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 404 | `{"message":"Message not found"}` |
| 500 | `{"message":"Failed to remove emoticon"}` |

#### 6.11.3 `GET /api/messages/:messageId/emoticons` — Auth: **yes, member-only**

Server-side aggregation: replays **all** rows with `target_message_id = :messageId`
`ORDER BY msg_serial ASC`, applying `emoticon_add` → `set.add(sender)`,
`emoticon_remove` → `set.delete(sender)`. Codes whose set ends empty are omitted.

Result order follows first-appearance order of each `emoticonCode`.

| Status | Body |
|---|---|
| 200 | `EmoticonAggregation[]` (possibly `[]`) |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 404 | `{"message":"Message not found"}` |
| 500 | `{"message":"Failed to get emoticons"}` |

**Not block-filtered** — a blocker sees blocked users in the `users` array here, even though
their events are hidden from `/sync`. Clients that fold reactions from `/sync` (the recommended
path) get the block-filtered view instead; the two can disagree. **Recommendation:** apply the
viewer's block list to this endpoint in PocketSkynet.

---

### 6.12 Sync & Read State (4)

#### 6.12.1 `GET /api/rooms/:roomId/sync?since=<serial>` — Auth: **yes, member-only**

The primary real-time endpoint. See §8.

| Param | Type | Default | Semantics |
|---|---|---|---|
| `since` | serial string | `"0"` | `msg_serial > since` (strictly greater). `NaN`/`<0`/`>MAX_SAFE_INTEGER` → `0` |

Query: `WHERE room_id = ? AND msg_serial > ? [AND sender_address NOT IN (blocked)]
ORDER BY msg_serial ASC LIMIT 501`, LEFT JOIN `users`.

- Hard cap **500** rows per response (`SYNC_MESSAGE_LIMIT`); 501 are fetched to compute `hasMore`.
- `hasMore` is returned **only** in the `X-Has-More: true|false` response header — the body
  stays a plain JSON array so existing clients keep working. The header is CORS-exposed.
- **Unlike `/messages`, nothing is filtered by type or `isDeleted`.** Deleted rows (`msgType:
  "delete"`, empty content), `delete_all` markers, and both emoticon types are all delivered.
  That is exactly what makes incremental folding correct.

| Status | Body |
|---|---|
| 200 | `MessageWithSender[]` ascending by `msgSerial`, plus `X-Has-More` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to sync messages"}` |

Drain loop: while `X-Has-More == "true"`, immediately re-sync with `since = max(msgSerial seen)`.
This makes `since=0` (cold start) safe — it never dumps unbounded history in one response.

#### 6.12.2 `GET /api/rooms/:roomId/latest-serial` — Auth: **yes, member-only**

`SELECT msg_serial … ORDER BY msg_serial DESC LIMIT 1`, or `0` when the room is empty.
**Not** block-filtered — the value may exceed the highest serial the caller can actually see.
Use it as a change detector, not as a read cursor.

| Status | Body |
|---|---|
| 200 | `{"serial": 1749652746620}` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to get latest message serial"}` |

#### 6.12.3 `GET /api/rooms/:roomId/latest-timestamp` — Auth: **yes, member-only**

`SELECT message_timestamp … ORDER BY message_timestamp DESC LIMIT 1`, or `0`. Counts deleted
rows and emoticon events; not block-filtered. Legacy polling aid — prefer `latest-serial`.

| Status | Body |
|---|---|
| 200 | `{"timestamp": 1749652746620}` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to get latest message timestamp"}` |

#### 6.12.4 `POST /api/rooms/:roomId/read` — Auth: **yes, member-only**

```json
{ "lastReadSerial": 1749652746620 }
```

`lastReadSerial`: required int, `0 … Number.MAX_SAFE_INTEGER`. Out of range → 400
`Validation failed` with `"Serial out of range"`.

Semantics (`storage.markRoomRead`), upserting on the unique `(room_id, user_address)` index:

- No existing row → insert with the given serial.
- Existing row and `lastReadSerial <= stored` → **no-op**, returns the stored value. The pointer
  **never moves backwards**.
- Otherwise update `last_read_serial` and `updated_at`.

Response contains only two fields (not the full row):

| Status | Body |
|---|---|
| 200 | `{"roomId":"room_…","lastReadSerial":1749652746620}` |
| 400 | `Validation failed` envelope |
| 403 | `{"message":"Access denied"}` |
| 500 | `{"message":"Failed to mark room as read"}` |

Typical flow: after rendering a `/sync` batch, POST the highest `msgSerial` seen.

---

## 7. Authentication Flow (End to End)

### 7.1 Challenge → sign → login

```
Client                                        Server
  │                                             │
  │ POST /api/auth/challenge                    │
  │   { walletAddress }                         │
  │────────────────────────────────────────────▶│  addr = lowercase(addr)
  │                                             │  GC expired challenges
  │                                             │  nonce = 32 random bytes (hex)
  │                                             │  challengeId = uuid4
  │                                             │  INSERT auth_challenges (expires now+10min)
  │◀────────────────────────────────────────────│
  │   { challengeId, message, expiresAt }       │
  │                                             │
  │ signature = personal_sign(message)          │   EIP-191, wallet key
  │                                             │
  │ POST /api/auth/login                        │
  │   { walletAddress, username, challengeId,   │
  │     signature, publicKey?, publicKeySig? }  │
  │────────────────────────────────────────────▶│  DELETE…RETURNING challenge (single-use)
  │                                             │  expiry / address / signature checks
  │                                             │  resolve username
  │                                             │  verify key binding (if both key fields)
  │                                             │  upsert users row
  │                                             │  get-or-create encryption salt
  │                                             │  sign JWT (HS256, 30d)
  │◀────────────────────────────────────────────│
  │   { user, token, fruitnationWallet,         │
  │     encryptionSalt }                        │
```

Signing detail: `ethers.verifyMessage(message, signature)` = EIP-191 `personal_sign` over the
**exact UTF-8 bytes** of `challenge.message`, i.e. `keccak256("\x19Ethereum Signed Message:\n" ||
len(msg) || msg)`, recovering the address from a 65-byte `r‖s‖v` signature (`v` ∈ {27, 28} or
{0, 1}; ethers accepts both).

The challenge is **single-use and burned on any failed attempt** (it is consumed with
`DELETE … RETURNING` before validation). Every retry needs a fresh `POST /api/auth/challenge` —
and the 10/min challenge limiter caps retry rate.

### 7.2 JWT claims and expiry

```json
{ "walletAddress": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266", "iat": 1749652746, "exp": 1752244746 }
```

- Algorithm **HS256** (pinned on verify), secret = `JWT_SECRET`.
- `expiresIn: "30d"` → `exp = iat + 2 592 000`.
- The **only** claim the server reads is `walletAddress`. No `sub`, `iss`, `aud`, or `jti`;
  no server-side session, no revocation list, no refresh token. Logout is purely client-side.
- Because tokens are opaque bearer credentials with no revocation, a leaked token is valid for
  up to 30 days. WebSocket sockets are force-closed when their token's `exp` passes (§12).
- `walletAddress` in the claim is whatever `upsertUser` returned, i.e. always lowercase.

### 7.3 Encryption key derivation and binding

The E2EE identity key is **separate from** the Ethereum wallet key, deterministically derived
from a wallet signature so multiple devices converge.

**Step 1 — obtain the salt.** From the login response (`encryptionSalt`) or
`GET /api/auth/encryption-salt`. 64 lowercase hex chars. It is a **secret**, served only to its
owner; a public salt would let any dapp reconstruct the derivation message and phish the
signature that *is* the E2EE private key.

**Step 2 — derivation message (v2, salted).** Exact bytes:

```
FruitNation Encryption Key Derivation v2\n\nAddress: {lowercase_address}\nSalt: {salt}\nPurpose: End-to-end encryption only
```

**Step 3 — derive.**

```
sig                  = personal_sign(derivationMessage)     // EIP-191, RFC 6979 deterministic
encryptionPrivateKey = keccak256(sig_bytes)                 // 32 bytes, over the full 65-byte r‖s‖v
encryptionPublicKey  = secp256k1_pubkey(encryptionPrivateKey)  // uncompressed: "04" ‖ x ‖ y, 130 hex, NO 0x
```

**Step 4 — binding message** (`buildKeyBindingMessage`, exact bytes):

```
FruitNation Public Key Binding\n\nAddress: {lowercase_address}\nEncryption Public Key: {encryptionPublicKey}
```

**Step 5 — publish.** `publicKeySig = personal_sign(bindingMessage)` **with the wallet key**,
then `PUT /api/auth/encryption-key { publicKey, publicKeySig }` (or supply both in the login body).

**Step 6 — verify before wrapping.** Any client that fetches
`POST /api/users/public-keys` **MUST** recompute the binding message and recover the signer
before wrapping a room key to that `publicKey`. Skipping this lets a compromised server
substitute its own key at invite time and silently read the room. A `null` `publicKeySig` is
**not verifiable** → refuse to wrap.

**Legacy (pre-salt) derivation**, accepted only for healing old data — never use for new keys:

```
FruitNation Encryption Key Derivation\n\nAddress: {lowercase_address}\nPurpose: End-to-end encryption only
```

Canonical test vectors for the legacy path live in
`server/test/vectors/crypto-v2.json`. (`server/test/test-vectors.json` contains **wrong**
expected values for some entries and must not be used.)

---

## 8. The msgSerial Sync Model

### 8.1 What a serial is

`msgSerial` is a **per-room, strictly increasing, timestamp-derived 64-bit integer**. It is
**not** a dense `1,2,3…` sequence — values are near `Date.now()` in milliseconds and skip
arbitrarily. Never assume contiguity, never compute `count = maxSerial - minSerial`.

Every mutation that clients must observe bumps the row's serial:

| Operation | Row | New `msgType` | Serial |
|---|---|---|---|
| Send message | new row | `add` | fresh |
| Edit message | **same row** | `edit` | fresh (higher) |
| Delete message | **same row** | `delete` | fresh (higher) |
| Delete all | all rows removed + one new marker row | `delete_all` | fresh |
| Add reaction | new row | `emoticon_add` | fresh |
| Remove reaction | new row | `emoticon_remove` | fresh |
| Publish txHash | **same row**, `tx_hash` set | *(unchanged, stays `add`/`edit`)* | fresh (higher) |

Because edits/deletes/publishes reuse the row and only advance the serial, a client that syncs
from its stored high-water mark receives the **current** state of every changed message —
`/sync` is an idempotent state-transfer stream, not a strict event log. Two consequences:

- An edit is delivered as the whole updated row, not a diff.
- A message edited before you ever saw it arrives once, already edited, with `msgType: "edit"` —
  so a client folding `edit` must **upsert**, not require a pre-existing entry (the reference
  `PROTOCOL.md` pseudocode wrongly ignores edits for unknown IDs; see §8.4).

### 8.2 The sync loop

```
since := load_persisted_high_water_mark(roomId)   // 0 on cold start
loop {
    (events, hasMore) := GET /api/rooms/{roomId}/sync?since={since}   // reads X-Has-More
    for e in events { fold(e); since = max(since, e.msgSerial) }
    persist(since)
    if !hasMore { break }
}
```

Trigger the loop on: WebSocket `new_message` / `member_removed` / `rooms_updated`, app
foreground, and a slow poll fallback (60 s recommended; the 100 req/min/IP limit is the binding
constraint if you have many rooms open).

Because blocked senders' rows are filtered out of the response, their serials are simply skipped
— the high-water mark advances past them on the next visible event. If the newest events in a
room are *all* from blocked senders, `since` stops advancing until a visible event arrives; that
is harmless (the same rows are re-filtered on each poll) but means `latest-serial` may stay
permanently ahead of your cursor. Do not use `latest-serial == since` as a "caught up" test.

### 8.3 Serial assignment algorithm (server side)

`storage.getNextMessageSerial(roomId)`:

```
now          = Date.now()
latestSerial = SELECT msg_serial … WHERE room_id=? ORDER BY msg_serial DESC LIMIT 1  (0 if none)
lastGen      = in_process_map[roomId] ?? 0
next         = max(now, latestSerial + 1, lastGen + 1)
if next > 9007199254740991: throw "Message serial number exceeded safe integer range"
in_process_map[roomId] = next
return next
```

Properties and hazards to carry into the Rust port:

- Serials track wall-clock ms, so they are globally comparable across rooms in practice, but
  only **per-room** monotonicity is guaranteed.
- The `lastGen` map is **in-process memory**. With more than one server process/replica sharing
  one database, two writers can compute the same `next` in the same millisecond, producing
  duplicate serials — and a client using `msg_serial > since` would then **skip** one of them.
  **Recommendation for PocketSkynet: replace this with a database-side per-room counter** (a
  `rooms.next_serial` column bumped in the same transaction as the insert, or a Postgres
  sequence per room), preserving the "≥ now" property if you want serials to remain
  timestamp-like. This is the single most important correctness fix in the port.
- After `DELETE /api/rooms/:roomId/messages` empties the table, `latestSerial` reads back as 0;
  monotonicity then rests entirely on `now` and the in-process `lastGen`.
- `msgSerial` and `messageTimestamp` are independently assigned and can differ (an edit keeps
  its original timestamp but gets a new serial). **Order by `msgSerial` for sync; order by
  `messageTimestamp` for display.**

### 8.4 `/sync` vs `/messages`

| | `GET …/sync` | `GET …/messages` |
|---|---|---|
| Cursor | `since` = **msgSerial**, exclusive (`>`) | `since`/`before` = **messageTimestamp** (`>=` / `<`) |
| Order | `msgSerial` ASC | `messageTimestamp` ASC (query is DESC then reversed) |
| Page size | fixed 500, `X-Has-More` header | `limit` 1–100, default 50, no flag |
| Deleted rows | **included** (`msgType:"delete"`) | excluded (`is_deleted = false`) |
| Emoticon events | **included** | excluded |
| `delete_all` markers | **included** | excluded |
| Block filter | yes | yes |
| Purpose | incremental state transfer | initial load / backward paging |

Recommended client strategy: `GET /messages?limit=50` for the initial screen and for scroll-back,
`/sync` for everything live. Seed the sync cursor from `GET /rooms/:id/latest-serial` **only** if
you intend to ignore history; otherwise start at 0 and drain.

---

## 9. Message Event Types and Folding

`msgType` ∈ `{"add", "edit", "delete", "delete_all", "emoticon_add", "emoticon_remove"}`
(plus the never-written DB default `"message"`, which clients should alias to `"add"`).

| `msgType` | `id` refers to | Key fields | Fold action |
|---|---|---|---|
| `add` | the message itself | `content`, `msgHash`, `isEncrypted`, `iv`, `hmac`, `encVer`, `keyVersion`, `txHash` | upsert into the message map |
| `edit` | the **same** message | as above + `editedAt` non-null | **upsert** (replace content and all crypto fields) |
| `delete` | the same message | `content: ""`, `msgHash: ""`, `isDeleted: true`, `iv`/`hmac` null | remove from the map (or tombstone it) |
| `delete_all` | a **new marker row** | `content: ""`, `senderAddress` = whoever purged | **clear the entire room's local cache**, then keep folding subsequent events |
| `emoticon_add` | a new event row | `targetMessageId`, `emoticonCode`, `senderAddress` | `reactions[target][code].insert(sender)` |
| `emoticon_remove` | a new event row | same | `reactions[target][code].remove(sender)`; drop the code if the set empties |

Reference fold (correcting the two bugs in `PROTOCOL.md`'s pseudocode):

```
fn fold(state, e):
    match e.msgType:
      "add" | "message" | "edit" =>
          state.messages.insert(e.id, e)          # UPSERT — never "only if present"
      "delete" =>
          state.messages.remove(e.id)
          state.reactions.remove(e.id)            # drop orphaned reactions too
      "delete_all" =>
          state.messages.clear()
          state.reactions.clear()
      "emoticon_add" =>
          state.reactions[e.targetMessageId][e.emoticonCode].insert(e.senderAddress)
      "emoticon_remove" =>
          set = state.reactions[e.targetMessageId][e.emoticonCode]
          set.remove(e.senderAddress)
          if set.is_empty(): remove that code
      _ => ignore (forward compatibility — unknown types must not abort the batch)
    state.cursor = max(state.cursor, e.msgSerial)
```

Display order: sort the message map by `messageTimestamp` ASC (**not** by `msgSerial`, or edited
messages would jump to the bottom). Ties: break on `id` for stability.

Notes:

- Reactions can target a message you never received (blocked sender, or purged by `delete_all`).
  Keep them keyed by `targetMessageId` and render only when the target exists; do not drop them,
  since a later backfill may bring the target in.
- A `delete_all` arriving mid-batch must clear state **at that point in serial order**, not at
  the end of the batch — later events in the same batch are post-purge.
- `txHash` updates arrive as a re-delivery of the row with its original `msgType`; the upsert
  path handles them with no special case.
- Decryption: for `isEncrypted: true`, select the room key whose `keyVersion` matches the
  message's `keyVersion` (from `GET …/keys/versions`) and dispatch on `encVer`
  (1 = legacy, 2 = key-separated KDF + MAC over roomId/IV/ciphertext). If you lack that epoch's
  key, refetch `keys/versions`; if it is still absent, the message is permanently unreadable to
  you (you joined after that epoch) — render a placeholder, do not error the batch.

---

## 10. Room Key Epochs

### 10.1 State

Two columns on `rooms` drive everything:

| Field | Meaning |
|---|---|
| `currentKeyVersion` | The epoch new encrypted messages **must** be sealed under. Starts at 1 |
| `keyRotationPending` | `true` ⇒ someone left/was kicked and the key has not been re-keyed yet. **Blocks all new encrypted messages** (409) |

`room_keys` holds one row per `(room, user, epoch)` — a member accumulates one wrap per epoch
they can read, which is what preserves their access to history across rotations.

### 10.2 Lifecycle

```
Room created                 currentKeyVersion=1, keyRotationPending=false, no room_keys
    │
    ├─ creator generates AES-256 key, wraps to self
    │     POST /rooms/:id/keys { userAddress: self, …, keyVersion: 1 }
    │     → hasEncryption becomes true
    │
    ├─ admin invites bob
    │     POST /users/public-keys ["0xbob"] → verify publicKeySig ← MANDATORY
    │     POST /rooms/:id/invite  { userAddress: 0xbob }
    │     POST /rooms/:id/keys    { userAddress: 0xbob, …, keyVersion: 1 }   ← pre-wrap while online
    │     (bob is an "invitee", accepted by check #2 of §6.9.1)
    │
    ├─ bob accepts → member; GET /rooms/:id/keys/versions returns his epoch-1 wrap
    │
    ├─ bob leaves / is kicked
    │     server: DELETE all of bob's room_keys rows (every epoch)
    │             keyRotationPending = true
    │     → every POST …/messages with isEncrypted:true now 409s KEY_ROTATION_REQUIRED
    │
    └─ any remaining member rotates
          GET /rooms/:id/members                 → current roster
          POST /users/public-keys [roster]        → keys + verify every binding
          generate NEW AES-256 key
          POST /rooms/:id/rotate-key { newVersion: current+1, keys: [wrap per member] }
          server (one transaction): insert wraps, currentKeyVersion++, keyRotationPending=false
          WS broadcast {"type":"new_message"} → members refetch keys/versions
```

### 10.3 Client rules

1. **Before sending an encrypted message**, ensure your `keyVersion` equals the room's
   `currentKeyVersion` (from `GET /api/rooms` or `GET /api/rooms/:roomId`). On
   `STALE_KEY_VERSION`, refetch `keys/versions`, re-encrypt under the epoch named in the 409's
   `currentKeyVersion`, and retry once.
2. **On `KEY_ROTATION_REQUIRED`**, do not retry — perform a rotation (any member may) or wait for
   another member to. Then re-encrypt under the new epoch.
3. **Decrypting history** requires all epochs: always use `GET …/keys/versions`, not `GET …/keys`
   (which returns only the latest wrap).
4. **Never wrap to an unverified public key.** Verify `publicKeySig` against the §7.3 binding
   message for every recipient, every time.
5. Rotation is all-or-nothing: the server rejects a rotation missing any current member (400 with
   `missing[]`) or naming a non-member (400). Fetch the roster immediately before rotating; a
   concurrent membership change yields one of those 400s or a 409 stale-version — refetch and retry.
6. Unencrypted rooms ignore epochs entirely: `keyVersion` defaults to 1, no gate applies, and
   `hasEncryption` stays `false` until the first `room_keys` row exists.

### 10.4 Crypto parameters (for interop)

| Layer | Parameters |
|---|---|
| Message body | AES-256-CBC, PKCS#7, 16-byte per-message IV (hex in `iv`), HMAC-SHA256 in `hmac` (encrypt-then-MAC) |
| Room key wrap | secp256k1 ECDH → AES-256-CBC with a 16-byte IV (`encryptionIV`), HMAC-SHA256 (`hmac`), ephemeral pubkey in `ephemeralPublicKey` (uncompressed, 130 hex) |
| `encVer: 1` | legacy: shared secret used directly as key material |
| `encVer: 2` | key-separated KDF (distinct enc/MAC keys) + MAC covering roomId, IV, and ciphertext |
| `msgHash` | `sha256_hex(content_as_sent)` — of the **ciphertext** for encrypted messages |

Validate any implementation against `server/test/vectors/crypto-v2.json` (canonical).

---

## 11. Blocking Semantics

Storage is a single directed row `blocker → blocked`. Enforcement is a patchwork of directed and
bidirectional checks; the table below is exhaustive. "A blocks B" throughout.

| Surface | Effect | Direction |
|---|---|---|
| `GET /api/users/search` | B is absent from A's results **and** A is absent from B's results | **bidirectional** |
| `GET /api/rooms/:id/messages` | B's messages are absent from A's responses. A's messages are still visible to B | directed (viewer-side) |
| `GET /api/rooms/:id/sync` | B's events (messages, edits, deletes, reactions) never reach A. A's events still reach B | directed (viewer-side) |
| `POST /api/rooms/:id/messages` | **B can still post.** Other members see it; only A does not | — |
| `POST /api/rooms/:id/invite` | A cannot invite B (403 *"you have blocked"*); B cannot invite A (403 *"have blocked you"*) | **bidirectional** |
| WebSocket `typing` relay | Neither direction receives the other's typing signal | **bidirectional** |
| `GET /api/users/:address` | No effect — A can read B's profile and vice versa | none |
| `GET /api/rooms/:id/members` | No effect — B appears in the roster A sees | none |
| `GET /api/rooms/:id/admins` | No effect | none |
| `GET /api/messages/:id/emoticons` | **No effect** — B appears in `users[]`. Inconsistent with `/sync` | none *(fix in port)* |
| `unreadCount` on `GET /api/rooms` | **No effect** — B's messages inflate A's unread badge for messages A can never fetch | none *(fix in port)* |
| `GET /api/rooms/:id/latest-serial` / `latest-timestamp` | No effect | none |
| `GET /api/users/blocked-by` | B can enumerate everyone who blocked them, including A | — |
| Room membership | Blocking does **not** remove either party from shared rooms | — |

Implementation detail: the read filter is `sender_address NOT IN (SELECT blocked_address FROM
blocked_users WHERE blocker_address = viewer)`, applied as a SQL `NOT IN` when the blocked set is
non-empty. Addresses are compared lowercase throughout.

**Recommendations for PocketSkynet:**
- Apply the viewer's block set to `unreadCount` and to `GET /messages/:id/emoticons` so all
  read surfaces agree.
- Keep `blocked-by` (native clients need it for symmetric filtering) but document that it is
  observable by the blocked party.

---

## 12. WebSocket Channel

```
ws(s)://{host}/ws
```

The socket is a **wake-up signal only** — message content is never delivered over it. On any
event, the client refetches over REST so the same membership/blocking rules apply. If the socket
fails, the app still works via polling; only latency degrades.

### 12.1 Handshake and auth

Preferred (keeps the token out of URLs and proxy logs):

```
Sec-WebSocket-Protocol: fnauth, <JWT>
```

The server accepts the handshake only if `fnauth` is among the offered protocols, and echoes
back **`fnauth`** (never the token). In browsers: `new WebSocket(url, ["fnauth", token])`. The
token is read from whichever offered protocol is not `fnauth`.

Fallback for clients that cannot set a subprotocol: `ws://host/ws?token=<JWT>`.

> **PocketSkynet divergence.** The reference server accepts `?token=` unconditionally.
> PocketSkynet gates it behind `--sse-token-query` (default **off**) on `/ws` as well as on
> `/api/events`, because a full-lifetime bearer token in a URL is recorded by access logs,
> proxy logs, `Referer` headers and browser history — and the exposure is not transport-specific,
> so gating only SSE would be arbitrary. With the flag off, a `?token=` handshake is rejected
> **401** with a message naming the flag, rather than the generic `Invalid token`.

> Reverse-proxy warning: many proxies drop `Sec-WebSocket-Protocol`, silently breaking
> subprotocol auth. If the proxy cannot be fixed, start the server with `--sse-token-query`
> and use the `?token=` fallback.

JWT verification pins HS256 and reads `walletAddress` + `exp`.

### 12.2 Close codes

| Code | Reason | Cause |
|---|---|---|
| 4001 | `Missing token` / `Invalid token` / `Token expired` | no token; verify failed; `exp` passed while connected (checked every 30 s) |
| 4008 | `Idle timeout` | 3 minutes with no activity |
| 4013 | `Too many connections` / `Server at capacity` | >8 sockets for one wallet; ≥5000 total |
| 4500 | `Server configuration error` / `Failed to load rooms` | `JWT_SECRET` unset; room lookup threw |
| 1001 | `Server shutting down` | graceful shutdown |

Reconnect with exponential backoff and a fresh token.

### 12.3 Limits

| Limit | Value |
|---|---|
| Max frame payload | 16 KB (larger frames are rejected) |
| Sockets per wallet | 8 |
| Total sockets | 5000 |
| Idle timeout | 180 s (reset by any activity, including received notifications) |
| Server ping interval | 30 s; terminate after 2 consecutive missed pongs (~60 s grace) |
| Typing relay throttle | ≤1 per second **per socket**, server-enforced |
| `perMessageDeflate` | disabled |

### 12.4 Server → client events

| Event | Emitted by | Client action |
|---|---|---|
| `{"type":"new_message","roomId":"…"}` | send / edit / delete / delete-all / emoticon add+remove / **rotate-key** | `/sync` the room; for encrypted rooms **also refetch `keys/versions`** (the epoch may have advanced) |
| `{"type":"rooms_updated"}` | `refreshUserRooms` — accept invitation, leave, kick | refetch `GET /api/rooms` |
| `{"type":"member_removed","roomId":"…"}` | leave, kick, **and accept-invitation** | refresh the room's members/details. Despite the name it means *"roster changed"* |
| `{"type":"invitation_received","roomId":"…"}` | invite | refetch `GET /api/invitations` |
| `{"type":"typing","roomId":"…","from":"0x…"}` | another member's typing relay | show indicator; expire after ~4 s of silence |
| `{"type":"pong"}` | reply to a client `ping` | reset keepalive |

Delivery is fan-out to every socket subscribed to the room, where subscriptions are the room-ID
set loaded at connect time and refreshed by `refreshUserRooms`. Room creation and room deletion
emit **nothing** — clients must refetch on their own.

### 12.5 Client → server messages

| Message | Effect |
|---|---|
| `{"type":"ping"}` | resets the 3-minute idle timer, clears missed-ping count, replies `{"type":"pong"}`. Send every ~25 s |
| `{"type":"typing","roomId":"…"}` | relays `typing` to the room's other connected members. Server enforces membership (via the subscription set) and the 1/s throttle; clients should self-throttle to ~1 per 2 s |

Non-JSON frames and unknown types are silently ignored. Typing is filtered by blocks in **both**
directions and never echoes to the sender's own sockets.

---

## 13. Unread Counts and Read State

- `room_reads(room_id, user_address, last_read_serial)` — unique per pair; the pointer is
  **monotonic** (a lower serial is ignored, returning the stored value).
- `unreadCount` (only on `GET /api/rooms`) counts rows where
  `msg_serial > last_read_serial AND msg_type = 'add' AND is_deleted = false AND sender_address <> me`.
  Edits, deletes, `delete_all`, and reactions therefore **never** create unread badges, and your
  own messages never count.
- No `last_read_serial` row ⇒ `lastReadSerial = 0` ⇒ everything counts as unread.
- Leaving or being kicked **deletes** the read row (and the hidden-room row) for that member.
- Deleting a room deletes all its read rows.
- **[QUIRK]** the count is **not** block-filtered (§11).

Client recipe: after rendering a `/sync` batch, `POST /api/rooms/:id/read` with the highest
`msgSerial` in that batch (regardless of type). Since `unreadCount` only counts `add` rows, a
pointer advanced past an `edit` costs nothing.

---

## 14. Endpoint Index

| # | Method | Path | Auth | Authorization |
|---|---|---|---|---|
| 1 | GET | `/api/health` | — | — (exempt from rate limiting) |
| 2 | GET | `/api/blockchain/info` | — | — |
| 2a | GET | `/api/networks` | — | — (PocketSkynet extension) |
| 2b | POST | `/api/images` | ✓ | any user (5 MB body cap, PocketSkynet extension) |
| 2c | GET | `/api/images/{name}` | — | capability URL (PocketSkynet extension) |
| 3 | POST | `/api/auth/challenge` | — | — (10/min) |
| 4 | POST | `/api/auth/login` | — | — (5/min) |
| 5 | POST | `/api/auth/logout` | — | — |
| 6 | GET | `/api/auth/encryption-salt` | ✓ | self |
| 7 | PUT | `/api/auth/encryption-key` | ✓ | self + signature binding |
| 8 | GET | `/api/auth/profile` | ✓ | self |
| 9 | PUT | `/api/auth/profile` | ✓ | self |
| 10 | GET | `/api/users/search?q=` | ✓ | block-filtered (both directions) |
| 11 | GET | `/api/users/:address` | ✓ | any authenticated user |
| 12 | POST | `/api/users/public-keys` | ✓ | any authenticated user |
| 13 | GET | `/api/users/blocked` | ✓ | self |
| 14 | GET | `/api/users/blocked-by` | ✓ | self |
| 15 | POST | `/api/users/block` | ✓ | self (target must exist, ≠ self) |
| 16 | DELETE | `/api/users/block/:address` | ✓ | self |
| 17 | GET | `/api/users/:address/is-blocked` | ✓ | self |
| 18 | POST | `/api/rooms` | ✓ | any authenticated user |
| 19 | GET | `/api/rooms` | ✓ | own memberships, hidden excluded |
| 20 | GET | `/api/rooms/hidden` | ✓ | own hidden rooms, membership re-checked |
| 21 | POST | `/api/rooms/:roomId/hide` | ✓ | **member** |
| 22 | DELETE | `/api/rooms/:roomId/hide` | ✓ | none (always succeeds) |
| 23 | GET | `/api/rooms/:roomId` | ✓ | **member** |
| 24 | PATCH | `/api/rooms/:roomId` | ✓ | **admin** |
| 25 | DELETE | `/api/rooms/:roomId` | ✓ | **admin** |
| 26 | POST | `/api/rooms/:roomId/leave` | ✓ | none *(should be member — see §6.5.6)* |
| 27 | POST | `/api/rooms/:roomId/kick` | ✓ | **admin**, not self |
| 28 | GET | `/api/rooms/:roomId/members` | ✓ | **member** |
| 29 | POST | `/api/rooms/:roomId/invite` | ✓ | **admin**, block-gated both ways |
| 30 | GET | `/api/invitations` | ✓ | self |
| 31 | POST | `/api/invitations/:roomId/accept` | ✓ | invitee |
| 32 | POST | `/api/invitations/:roomId/decline` | ✓ | invitee |
| 33 | POST | `/api/rooms/:roomId/admins` | ✓ | **admin**, max 9 |
| 34 | DELETE | `/api/rooms/:roomId/admins/:walletAddress` | ✓ | **admin**, min 1 |
| 35 | GET | `/api/rooms/:roomId/admins` | ✓ | **member** |
| 36 | POST | `/api/rooms/:roomId/keys` | ✓ | self-key, or **admin** for others (no overwrite) |
| 37 | GET | `/api/rooms/:roomId/keys` | ✓ | **member**, own key only |
| 38 | GET | `/api/rooms/:roomId/keys/versions` | ✓ | **member**, own keys only |
| 39 | POST | `/api/rooms/:roomId/rotate-key` | ✓ | **member** (any), full coverage required |
| 40 | POST | `/api/rooms/:roomId/messages` | ✓ | **member** + epoch gate |
| 41 | GET | `/api/rooms/:roomId/messages` | ✓ | **member**, block-filtered |
| 42 | DELETE | `/api/rooms/:roomId/messages` | ✓ | **member** (any) |
| 43 | PATCH | `/api/messages/:messageId` | ✓ | **member** + message **owner** |
| 44 | DELETE | `/api/messages/:messageId` | ✓ | **member** (any) |
| 45 | POST | `/api/messages/:messageId/publish` | ✓ | message **sender** |
| 46 | POST | `/api/messages/:messageId/emoticons` | ✓ | **member** |
| 47 | DELETE | `/api/messages/:messageId/emoticons/:emoticonCode` | ✓ | **member** |
| 48 | GET | `/api/messages/:messageId/emoticons` | ✓ | **member** |
| 49 | GET | `/api/rooms/:roomId/sync` | ✓ | **member**, block-filtered |
| 50 | GET | `/api/rooms/:roomId/latest-serial` | ✓ | **member** |
| 51 | GET | `/api/rooms/:roomId/latest-timestamp` | ✓ | **member** |
| 52 | POST | `/api/rooms/:roomId/read` | ✓ | **member** |
| — | WS | `/ws` | ✓ (subprotocol or `?token=`) | subscribed to own rooms |

### 14.1 Route-precedence requirements

Static segments **must** win over parameterized ones, or these collide:

```
/api/users/search        before  /api/users/:address
/api/users/blocked       before  /api/users/:address
/api/users/blocked-by    before  /api/users/:address
/api/rooms/hidden        before  /api/rooms/:roomId
```

`axum`'s router already prefers static segments over `:param`, so registration order is not
load-bearing there — but verify, because Express's first-match semantics are what the reference
depends on. Note `/api/users/public-keys` (POST) does not collide with `/api/users/:address`
(GET) by method.

---

## 15. Consolidated Quirk / Divergence List

Each item: reference behavior → recommended PocketSkynet behavior. "Wire-visible" marks changes
existing clients could notice.

| # | Area | Reference behavior | Recommendation | Wire-visible |
|---|---|---|---|---|
| 1 | `POST /rooms/:id/leave` | No membership check; any user can set `keyRotationPending` on any room → DoS on encrypted messaging | Require membership, 403 otherwise | no (legit clients only leave rooms they are in) |
| 2 | `msgSerial` | In-process `lastGen` map; duplicate serials across replicas ⇒ dropped messages | DB-side per-room counter in the insert transaction | no |
| 3 | Login | `publicKeySig` set to NULL on every login that omits `publicKey` | Leave both key columns untouched when `publicKey` is absent | no |
| 4 | `blocked_users`, `hidden_rooms`, `room_members`, `room_admins` | No unique index; `onConflictDoNothing` is a no-op ⇒ duplicate rows | Add unique indexes; make inserts idempotent | list dedup only |
| 5 | `POST /users/public-keys` | Validation errors return **500** | Return 400 `Validation failed` | yes (error code) |
| 6 | `POST/DELETE /rooms/:id/hide` | Invalid `roomId` returns **500** | Return 400 `Validation failed` | yes (error code) |
| 7 | `PATCH /messages/:id` | No epoch gate; encrypted message can be silently downgraded to plaintext when `iv`/`hmac` omitted | Apply the same 409 gates as `POST`; reject a downgrade for an `isEncrypted` message | yes (new 409s) |
| 8 | `GET /rooms/:id/messages` | `LIMIT` applied before the msgType filter ⇒ short pages | Filter in SQL so pages are full | no |
| 9 | `unreadCount` | Not block-filtered | Exclude blocked senders | count values |
| 10 | `GET /messages/:id/emoticons` | Not block-filtered | Exclude blocked reactors | `users[]` contents |
| 11 | `DELETE /rooms/:id` | `room_invitations` orphaned; not transactional | Delete invitations too, in one transaction | no |
| 12 | `GET /invitations` | Filters on the literal string `"(deleted room)"` | Filter on room-missing | no |
| 13 | `POST /messages/:id/publish` | All errors are 400, incl. not-found and authz; deleted messages publishable when `FN_RPC_URL` unset | Use 403/404 appropriately; always exclude deleted messages | yes (error codes) |
| 14 | `DELETE …/emoticons/:emoticonCode` | Double `decodeURIComponent` | Decode once | only for codes containing `%` |
| 15 | `POST /messages/:id/emoticons` | Dead "already added this emoticon" 400 branch | Omit, or implement a real duplicate check | no |
| 16 | Auth header | `replace("Bearer ", "")` accepts bare tokens, is case-sensitive | Parse the scheme case-insensitively; optionally keep bare-token support | no |
| 17 | `lastMessage` | `delete_all` markers can surface as `lastMessage` | Exclude `delete_all` | preview text |
| 18 | Sender fallback | `getMessages` omits `publicKeySig`, `/sync` includes it | Always emit `publicKeySig: null` | added null field |
| 19 | `GET /users/search` | Unbounded, unordered | Cap (e.g. 50) and order by `username` | result size |
| 20 | Zod trim ordering | `"   "` passes `min(1)` then stores `""` | Trim **before** length validation; reject empty | yes (new 400s) |
| 21 | `msgType` | DB default `"message"` is never written | Alias `"message"` → `"add"` on read; always write `"add"` | no |
| 22 | `POST /rooms/:id/keys` | Does not bump `currentKeyVersion` (by design) | Keep; epoch changes only via `/rotate-key` | no |

---

## 16. Paid Features (PocketSkynet extensions)

Two features are gated by an **on-chain payment to the operator's wallet**
(`VITE_FRUITNATION_WALLET`) on the configured chain (`VITE_CHAIN_ID`). The
payment is the business model — it funds the deployment — and the spam gate:
broadcasting to every screen or parking bytes on the operator's disk costs
real money.

The flow is always: the client signs and broadcasts a native transfer in the
browser (the server never sees a key), waits for the receipt, then presents
the transaction hash. The server verifies via JSON-RPC on the configured
chain's endpoint:

1. `eth_getTransactionByHash` — exists, mined, `to` = the operator wallet,
   `from` = the **authenticated caller**, `value` ≥ the feature's price.
2. `eth_getTransactionReceipt` — `status == 0x1`.
3. The `payments` table — the hash is single-use (`PRIMARY KEY`), across
   *both* features. 409 on reuse.

`--no-payment-verify` (`PS_NO_PAYMENT_VERIFY`) skips the RPC steps for tests
and offline development — format and single-use checks still run — and is
**refused when `PS_ENV=production`**, exactly like `--no-rate-limit`.

Prices are decimal-CRO strings, served by `GET /api/blockchain/info` as
`shoutPriceCro` (default `"10"`, env `PS_SHOUT_PRICE_CRO`) and
`publishPriceCro` (default `"1"`, env `PS_PUBLISH_PRICE_CRO`), so the number
on the client's pay button is the number the server enforces.

### 16.1 Shout — paid broadcast

Pay ≥ `shoutPriceCro` and a line of text lands on **every connected user's**
screen for up to 60 seconds. A shout is not a message: it belongs to no room,
is never encrypted or persisted as content, and each viewer may dismiss it
locally (the server never closes a shout early). Rows outlive expiry as the
operator's revenue ledger only.

#### `POST /api/shout` — Auth: **yes**

```json
{ "text": "We are live! 🎉", "txHash": "0x…64 hex…", "durationSecs": 60 }
```

| Field | Rule |
|---|---|
| `text` | 1–200 chars after trim, single line, no control chars, Unicode OK |
| `txHash` | `0x` + 64 hex (any case; normalised lowercase); verified + burned |
| `durationSecs` | optional; clamped to **[5, 60]**, default 60 |

Checks in order: text/hash shape (400) → active-shout cap (**3 per wallet**,
400) → payment verification (400/409) → insert → audit (`shout_broadcast`) →
realtime fan-out. 200 returns the `Shout` object:

```json
{ "id": "shout_…", "senderAddress": "0x…", "username": "alice",
  "text": "We are live! 🎉", "createdAt": 1753…, "expiresAt": 1753…,
  "txHash": "0x…", "amountWei": "10000000000000000000" }
```

The realtime event is a wake-up like every other — `{"type":"shout",
"shoutId":"…"}` on a new **`Target::All`** (every connection, still
block-filtered by origin). It is **not replayable**; clients fetch the active
set on connect and on each event. Polling clients pick shouts up through
their periodic safety sync.

#### `GET /api/shout/active` — Auth: **yes**

`{"shouts": [Shout, …]}` — unexpired only, newest first.

### 16.2 Web publishing — paid hosting

Pay ≥ `publishPriceCro` and the server hosts a page at **`/sites/{id}/`**:
a single HTML document (pasted or uploaded) or a zip carrying `index.html`
plus assets. Recorded in `published_sites`, indexed into search as kind
`site` (globally visible, like knowledge). **Any signed-in user may delete
any site** — the wall is shared and the community prunes it.

#### `POST /api/sites?title=…&txHash=…` — Auth: **yes**

Raw body (metadata in the query string, like attachments). Zip is detected by
magic bytes (`PK\x03\x04`); anything else is stored as `index.html`. Body cap
**25 MB**; unpacked cap **64 MB**; **500** files per site; **500** sites per
server; `title` 1–100 chars, room-name character policy.

Zip handling: `enclosed_name` refuses traversal (the whole upload fails);
`__MACOSX/` and dotfiles are silently dropped; a single wrapping folder is
stripped; `index.html` must exist at the (effective) root; duplicate paths
are refused. The upload is parsed **before** the payment is burned, so a bad
zip costs nothing. 201 returns the `Site` object (`id`, `ownerAddress`,
`username`, `title`, `txHash`, `amountWei`, `sizeBytes`, `fileCount`,
`createdAt`, `url`).

#### `GET /api/sites?limit=` — Auth: **yes**

`{"sites": [Site, …], "shareBase": "https://100.120.4.113:9777" | null}` —
newest first (limit 1–500, default 100). `shareBase` is the base URL other
devices should use to reach this server, with the startup banner's
preference order: the VPN (Tailscale, `100.64.0.0/10`) address first, then a
LAN address; `null` when the server is bound to loopback or its port is not
knowable. Clients prefix it onto each site's relative `url` to display and
copy a shareable address, falling back to their own origin when `null`.

#### `DELETE /api/sites/{id}` — Auth: **yes** (any user)

Removes the row (serving stops immediately), the search document, and the
directory. Audited (`site_removed`, with the deleter and the owner).

#### `GET /sites/{id}/{*path}` — Auth: **no**

The hosting itself, outside `/api`. `{id}` is exactly 32 lowercase hex;
`/sites/{id}` redirects to `/sites/{id}/`; a path ending in `/` serves its
`index.html`. Real content types for the usual extensions, octet-stream
otherwise; `Cache-Control: no-cache` (sites are deletable at any moment).

**Security:** every response carries
`Content-Security-Policy: sandbox allow-scripts allow-forms allow-popups
allow-modals allow-downloads` — **without `allow-same-origin`** — so a
published page runs in an opaque origin: scripts work, but the app's
`localStorage` (which may hold an opt-in recovery phrase), cookies, and
same-origin credentials do not exist there. The global
`X-Frame-Options: DENY` keeps published pages out of iframes; they open as
top-level tabs. This preserves the `routes/files.rs` invariant that uploaded
HTML never executes *as* this origin.

### 16.3 Search integration

`GET /api/search` accepts `kind=site`; site documents are `title` + the
readable text of `index.html` (tags/scripts/styles stripped, capped at
2000 chars), visible to every signed-in user.
