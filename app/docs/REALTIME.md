# Realtime Transport — Reference Behaviour + PocketSkynet Design

Part 1 (§1–§7) documents the **existing FruitNation server** exactly as implemented in
`server/server/websocket.ts` (authoritative), `server/client/src/services/websocket.ts`,
`server/client/src/hooks/usePolling.ts`, `server/client/src/services/connectionMode.ts`, and the
"WebSocket Notifications" section of `server/PROTOCOL.md`.
Part 2 (§8–§10) is the PocketSkynet (axum + Yew/WASM) design: WebSocket + SSE + JSON + JSONL.

---

# Part 1 — Reference behaviour (FruitNation Express server)

## 1. WebSocket handshake

| Property | Value |
|---|---|
| Path | `/ws` (exact; `WebSocketServer({ server, path: "/ws" })`) |
| URL | `ws://{host}/ws`, `wss://{host}/ws` |
| Subprotocol | `fnauth` (marker) — server echoes `fnauth` only |
| `perMessageDeflate` | disabled |
| `maxPayload` | 16384 bytes |
| Auth timing | at handshake/`connection`; **no post-connect auth message** |

### JWT transport — two accepted forms

**1. Preferred — `Sec-WebSocket-Protocol` (token never in the URL).**

```
Sec-WebSocket-Protocol: fnauth, <JWT>
```

Browser: `new WebSocket(url, ["fnauth", token])`.
Server logic (`handleProtocols`): if the offered set contains `fnauth`, select and echo
`fnauth`; otherwise **fail the handshake** (`return false`). Token extraction: split the raw
header on `,`, trim, drop empties, take the **first element that is not `fnauth`**.

**2. Fallback — query parameter.** `ws(s)://{host}/ws?token=<JWT>`. Read via
`url.searchParams.get("token")`. Only consulted when no non-`fnauth` subprotocol was offered.
Retained for native CLIs. Leaks the JWT into proxy/access logs — do not use in browsers.

> **PocketSkynet divergence.** The reference server accepts this unconditionally; PocketSkynet
> gates it behind `--sse-token-query` (default **off**), applying §8.1's SSE reasoning to `/ws`
> too — the URL-exposure problem is a property of putting a credential in a URL, not of the
> transport carrying it. With the flag off the handshake is rejected **401** with a message that
> names the flag, so a native CLI written against this section is told what to change instead of
> being pointed at its own credential.

Reverse-proxy hazard: proxies that strip `Sec-WebSocket-Protocol` silently break form 1
(handshake rejected, live updates stop). If the proxy cannot be fixed, start the server with
`--sse-token-query` and fall back to `?token=`.

### Verification

`jwt.verify(token, JWT_SECRET, { algorithms: ["HS256"] })` — algorithm pinned to defeat
alg-substitution (`alg:none`). Claims used: `walletAddress` (identity, lowercase as issued by
`/api/auth/login`), `exp` (optional; `0` means "no expiry claim", never enforced).

### Success

There is **no application-level "connected"/"ready" frame.** Success == the HTTP 101 completing
(with `Sec-WebSocket-Protocol: fnauth` echoed when form 1 was used). The server then:

1. Loads `storage.getUserRooms(walletAddress)` → the subscription set `info.rooms`.
2. Loads the block set (§5).
3. Registers the socket in `clients: Map<wallet, Set<WebSocket>>` and
   `clientInfo: Map<WebSocket, ClientInfo>`.
4. Arms the 3-minute idle timer.

The client's first real signal is either a server ping (≤30 s) or a notification event.

### Close codes (complete)

| Code | Reason string | When |
|---|---|---|
| `1001` | `Server shutting down` | `closeWebSocket()` graceful shutdown; all sockets |
| `4001` | `Missing token` | no subprotocol token and no `?token=` |
| `4001` | `Invalid token` | `jwt.verify` threw (bad sig, wrong alg, malformed, already expired) |
| `4001` | `Token expired` | periodic 30 s sweep found `tokenExp > 0 && now > tokenExp` |
| `4008` | `Idle timeout` | 3 min with no qualifying activity |
| `4013` | `Server at capacity` | `clientInfo.size >= 5000` at connect (checked **before** auth) |
| `4013` | `Too many connections` | wallet already has ≥8 open sockets |
| `4500` | `Server configuration error` | `JWT_SECRET` unset |
| `4500` | `Failed to load rooms` | `getUserRooms`/block load threw |
| `1006` | (none — abnormal) | `ws.terminate()` after >2 missed pings; no close frame is sent |
| `1000`/`1005` | — | client-initiated `ws.close()` (logout / intentional disconnect) |

Handshake failure via `handleProtocols === false` yields an HTTP-level rejection, **not** a close
code — the client sees an `error` then `close` with `1006`.

---

## 2. Message catalogue

All frames are UTF-8 JSON objects with a discriminating `"type"` string. Non-JSON frames are
silently ignored (`try { JSON.parse } catch {}`) — never an error reply. Unknown `type` values are
also silently ignored on both sides.

### 2.1 Client → Server (only two types exist)

**`ping`** — keepalive.

```json
{ "type": "ping" }
```

Effect: resets the idle timer, sets `missedPings = 0`, replies with `{"type":"pong"}`.
Reference client sends every 25 000 ms (must be < the server's 30 s ping tick).

**`typing`** — typing-indicator relay.

```json
{ "type": "typing", "roomId": "room_1749652739650_304e0eaf-…" }
```

Requires `typeof msg.roomId === "string"`. Server-side gates, in order:

1. `info.rooms.has(roomId)` — membership from the connect-time subscription set; silently
   dropped otherwise (no error frame).
2. Throttle: `Date.now() - info.lastTypingAt < 1000` → dropped silently.
3. `lastTypingAt = now`, idle timer reset.
4. Fan-out to every other socket that passes the §5 block filter.

Client self-throttles to one per 2000 ms (`sendTyping`) on top of the server's 1/s cap.

**There is no `subscribe` / `unsubscribe` message.** Room subscription is **entirely
server-derived**: the set is materialised from `getUserRooms()` at connect and mutated only by the
server calling `refreshUserRooms(walletAddress)` after a membership change (invite accepted, join,
leave, kick). Clients cannot subscribe to a room they are not a member of, and cannot
unsubscribe. Per-room interest on the client is a purely local concern (`wsService.onMessage(roomId, cb)`
routes an already-delivered event to a handler map).

There is no client→server `pong` message: pongs are the **protocol-level** WebSocket control
frame, handled by `ws.on("pong")`, not JSON.

### 2.2 Server → Client (six types)

**`new_message`** — room content changed.

```json
{ "type": "new_message", "roomId": "room_…" }
```

Emitted by `notifyRoom()` for: message send, message edit, message delete, `delete_all`,
`emoticon_add`, `emoticon_remove`, **and room-key rotation** (`routes.ts` lines 1292, 1537, 1736,
1774, 1807, 1863, 1918). Carries **no content, no msgSerial, no sender** — see §6.
For encrypted rooms the client must also refetch room-key versions, since the epoch may have
advanced.

**`rooms_updated`** — the recipient's room list changed.

```json
{ "type": "rooms_updated" }
```

No `roomId`. Emitted by `refreshUserRooms()` via `notifyUser()`, after the server-side
subscription set has already been replaced. Client refetches `GET /api/rooms`.

**`member_removed`** — membership of a room changed (leave / kick).

```json
{ "type": "member_removed", "roomId": "room_…" }
```

Sent with `notifyRoom` to the **remaining** members. The reference client treats it identically to
`rooms_updated` (both trigger the room-list refresh).

**`invitation_received`** — a pending invitation arrived.

```json
{ "type": "invitation_received", "roomId": "room_…" }
```

Sent with `notifyUser` to the invitee only. Client refetches the invitations list.

**`typing`** — relayed typing signal.

```json
{ "type": "typing", "roomId": "room_…", "from": "0xabc…" }
```

`from` is the **sender's** wallet address as carried in their JWT (lowercase). This is the only
server→client event that names a user.

**`pong`** — reply to a client `ping`.

```json
{ "type": "pong" }
```

### 2.3 Delivery routing

| Helper | Selection | Side effects |
|---|---|---|
| `notifyRoom(roomId, ev)` | iterate **all** `clientInfo`; deliver where `info.rooms.has(roomId) && readyState === OPEN` | resets each recipient's idle timer |
| `notifyUser(wallet, ev)` | `clients.get(wallet.toLowerCase())`; all that socket set | resets each recipient's idle timer |

`notifyRoom` is O(total sockets) per event — no room→socket index exists. PocketSkynet must not
copy this (see §9).

---

## 3. Liveness

| Constant | Value | Source |
|---|---|---|
| `IDLE_TIMEOUT` | 180 000 ms (3 min) | server |
| `SERVER_PING_INTERVAL` | 30 000 ms | server |
| `MAX_MISSED_PINGS` | 2 (≈60 s grace before terminate) | server |
| client `PING_INTERVAL` | 25 000 ms | web client |
| client typing self-throttle | 2 000 ms | web client |

**Idle timer.** Armed at connect; `clearTimeout` + re-arm on: a client `ping`, an accepted
`typing`, and **delivery of any event to that socket** (`notifyRoom`/`notifyUser` both call
`resetIdleTimer`). So a socket in an active room stays alive without pinging. On expiry:
`ws.close(4008, "Idle timeout")`.

**Server ping loop** (`setInterval`, 30 s, cleared on `wss.on("close")`), per socket, in order:

1. If `tokenExp > 0 && Date.now()/1000 > tokenExp` → `removeClient(ws)` then `close(4001, "Token expired")`; skip.
2. `missedPings++`.
3. If `missedPings > 2` → `removeClient(ws)` then `ws.terminate()` (no close frame → peer sees `1006`); skip.
4. `ws.ping()` (protocol-level).

`ws.on("pong")` sets `missedPings = 0`. A client JSON `ping` also zeroes it, so an app-level
keepalive alone is sufficient even if the peer never answers protocol pings.

Note the ordering consequence: `missedPings` is incremented **before** the ping is sent, so a
freshly connected socket that never responds is terminated on the 3rd tick (~90 s).

**Client reconnect / backoff** (`services/websocket.ts`):

- `delay = min(1000 * 2^attempts, 30000)`; `attempts` increments per scheduled retry.
  Sequence: 1 s, 2 s, 4 s, 8 s, 16 s, 30 s, 30 s, …
- `MAX_ATTEMPTS = 20`, then it gives up permanently (no jitter, no reset timer).
- `attempts` resets to 0 on a successful `onopen`.
- Reconnect is skipped when `intentionalClose` is set (`disconnect()` / logout).
- Reconnect is also skipped when no stored JWT exists.
- **All** close codes are treated identically — there is no special handling for `4001`, so an
  expired-token close produces 20 futile retries with a stale token.
- `reconnect()` (user activity: send/edit/delete/manual sync) cancels the pending backoff timer
  and connects immediately.
- On a **re**-connect (`wasConnected === true`), `onReconnect` handlers fire → each open room
  runs an immediate `/sync` to close the gap. This is the only gap-recovery mechanism; the socket
  itself replays nothing.

---

## 4. Limits and DoS guards

| Guard | Limit | Enforcement point | Action on breach |
|---|---|---|---|
| Frame size | 16 KB (`maxPayload`) | `ws` library | frame rejected, socket closed by the library (`1009` Message Too Big) |
| Sockets per wallet | 8 | after JWT verify | `close(4013, "Too many connections")` |
| Global sockets | 5000 | **first** thing in `connection`, before auth | `close(4013, "Server at capacity")` |
| Typing relays | 1 per 1000 ms **per socket** | in the `typing` branch | silently dropped, no error, timer not updated |
| Typing membership | must be in `info.rooms` | in the `typing` branch | silently dropped |
| JWT lifetime | `exp` claim | 30 s ping sweep | `close(4001, "Token expired")` |
| Idle | 180 s | per-socket timer | `close(4008, "Idle timeout")` |
| Dead peer | >2 missed pings | 30 s ping sweep | `terminate()` → `1006` |

The global ceiling is checked before JWT verification, so an unauthenticated flood cannot make the
server do crypto work — but it also means legitimate users are rejected indistinguishably from
attackers. Per-wallet capping happens *after* verification (it needs the identity).

Everything the socket accepts is a tiny control message; 16 KB is generous by ~3 orders of
magnitude and exists purely as a memory guard.

---

## 5. Block filtering

**Structure.** `ClientInfo.blockSet: Set<string>` — the **union of both directions**, all
lowercased:

```ts
loadBlockSet(w) = { b.blockedAddress ∀ getBlockedUsers(w) } ∪ { b.blockerAddress ∀ getUsersWhoBlocked(w) }
```

i.e. "everyone I blocked" ∪ "everyone who blocked me". Failure to load degrades **open**
(`catch { return new Set() }` — no filtering rather than no connection).

**Loading.** Once at connect, in parallel with `getUserRooms`.

**Refresh.** `refreshUserBlocks(wallet)` re-runs `loadBlockSet` and overwrites `info.blockSet` on
every socket that wallet has open. `routes.ts` calls it for **both parties** on block
(lines 476–477) and unblock (lines 501–502), so the change takes effect on live sockets
immediately — no reconnect needed. Lookup key is `wallet.toLowerCase()`; JWTs are issued with a
lowercased `walletAddress`, so the maps line up. PocketSkynet must preserve that normalisation
invariant explicitly rather than by luck.

**What is actually filtered.** Only `typing`. The relay predicate is:

```ts
otherWs !== ws
  && otherInfo.walletAddress !== walletAddress          // skip self's other sockets
  && otherInfo.rooms.has(msg.roomId)
  && otherWs.readyState === OPEN
  && !info.blockSet.has(otherInfo.walletAddress)        // sender-side view
  && !otherInfo.blockSet.has(walletAddress)             // recipient-side view
```

Both directions are checked, and each side's own set is already bidirectional — deliberately
redundant so a stale set on one socket cannot leak a typing indicator across a block. Typing is a
presence side-channel; unfiltered it would reveal that a blocked user is active in a shared room.

**What is NOT filtered.** `new_message`, `member_removed`, `rooms_updated`,
`invitation_received` are delivered to all subscribed sockets regardless of blocks. This is safe
because those events are contentless wake-ups (§6): the blocker learns "something happened in this
room", then calls `/api/rooms/:id/sync`, which applies the authoritative filter server-side —
`storage.getMessagesSinceSerial(roomId, since, viewerAddress)` adds
`notInArray(messages.senderAddress, blockedByViewer)`. Blocked senders' rows are skipped and their
serials simply never appear; the viewer's cursor jumps past them on the next visible event.

Residual leak, inherited and worth fixing in PocketSkynet: a blocker still receives a
`new_message` wake-up caused solely by a blocked user's message, learns something happened, syncs,
and gets nothing. Timing-observable. PocketSkynet's hub should carry the originating sender in the
internal event and drop the fan-out per-recipient when blocked, so a blocked-only event produces
no wake-up at all.

---

## 6. Notification semantics — wake-up signals only

**No message content ever traverses the socket.** `notifyRoom`'s type is literally
`{ type: string; roomId: string }`. There is no `msgSerial`, no `messageId`, no sender, no
ciphertext. Consequences:

- The socket is not an ordering or delivery guarantee. It cannot be replayed and carries no
  cursor. Losing events costs latency, never data.
- All authorisation, block filtering, and E2EE handling live on the REST path — one code path,
  no duplicated policy on the socket.
- Coalescing is free: N notifications for a room collapse into one sync.

**Client reaction** (`usePolling.handleSync`):

```
on new_message(roomId):
  loop:
    GET /api/rooms/{roomId}/sync?since={lastSerial}
      Authorization: Bearer <JWT>
    -> 200, body = MessageWithSender[]  (plain array)
       header X-Has-More: "true" | "false"
    if body empty: break
    apply events by msgType:
       "add"                -> insert
       "edit"               -> patch content/editedAt/isEncrypted/iv/hmac/msgHash of existing id
       "delete"             -> mark isDeleted on existing id
       "delete_all"         -> wipe the room's cache entirely
       "emoticon_add"       -> reaction store
       "emoticon_remove"    -> reaction store
    lastSerial = max(msgSerial over the batch)
    persist lastSerial (IndexedDB `syncRecords`)
    if not hasMore: break
```

Server side: `since` defaults to 0; membership is required (403 → the client infers "kicked");
page size `SYNC_MESSAGE_LIMIT = 500` with one extra row fetched to compute `hasMore`; `hasMore` is
returned in the **`X-Has-More` header**, not the body, so the JSON body stays a plain array for
older CLIs. `msgSerial` is a per-room monotonic integer (millisecond-derived) — **not** globally
unique across rooms. That distinction drives the SSE cursor design in §8.

Secondary reactions: `new_message` also nudges the room-list refresh (unread badges, last-message
preview); `rooms_updated`/`member_removed` refetch `GET /api/rooms`; `invitation_received`
refetches invitations; `typing` shows an indicator that should expire ~4 s after the last event.

---

## 7. Polling fallback

Mode is a **user-selected, persisted preference**, not automatic degradation
(`services/connectionMode.ts`):

```ts
type ConnectionMode = "websocket" | "polling";
localStorage["fn_connection_mode"];   // default "websocket" (also on unreadable storage)
```

Change → `setConnectionMode` notifies listeners → `usePolling` tears down and restarts the room's
sync manager in the new mode.

**`websocket` mode.** WS connected; sync is event-driven only. `POLLING_INTERVAL_ACTIVE` is
**not** armed — the disconnect handler explicitly logs "waiting for auto-reconnect (no polling)".
Recovery relies entirely on §3's backoff plus the on-reconnect catch-up sync. If the 20 attempts
are exhausted, new messages only appear on user activity (`reactivateWs`) or a manual reload.

**`polling` mode.** `wsService.stopAutoReconnect()` + `disconnect()` if connected, then
`setInterval(handleSync, 10_000)` per room — a flat 10 s, no backoff, no jitter, no adaptive
idle interval. Only the currently open room is polled; room lists rely on TanStack Query refetches.

Both modes run the same initial load on mount: hydrate from IndexedDB cache → `/sync?since=cached`
→ if the room has no cache at all, full `GET /api/rooms/:id/messages` → loop while `hasMore`.

There is **no automatic websocket→polling failover**. PocketSkynet should add one (§8).

---

# Part 2 — PocketSkynet design

Requirement: *websocket, JSON, SSE*. WebSocket and SSE are two transports over **one** event
model; the JSONL log (§10) is the third view of the same events.

## 8. SSE endpoint design — `GET /api/events`

SSE is the fallback and the "simple client" path: one-way, HTTP/1.1-friendly, auto-reconnecting
in the browser, and — unlike the reference WS — **resumable**.

### 8.1 Authentication

`EventSource` cannot set headers. Three options, in descending preference:

1. **Short-lived ticket (recommended).** `POST /api/events/ticket` with the normal
   `Authorization: Bearer <JWT>` →

   ```json
   { "ticket": "evt_9f3c…", "expiresAt": 1753600000, "ttlSeconds": 30 }
   ```

   Then `GET /api/events?ticket=evt_9f3c…`. Properties: 32 bytes of CSPRNG entropy, **single
   use** (consumed atomically on connect), 30 s TTL, bound to the wallet and to the client IP,
   stored in a `DashMap<TicketId, (wallet, exp, ip)>` with a sweeper. A ticket in a proxy log is
   worthless within 30 s and worthless immediately after use, unlike a 24 h JWT.

2. **`fetch()` + `ReadableStream`** (Yew/WASM can do this; `EventSource` cannot). Sends
   `Authorization: Bearer` properly and needs no ticket. Costs a hand-rolled SSE framing parser
   (~80 lines) and loses the browser's built-in reconnect/`Last-Event-ID` handling — you
   reimplement both. Use this for the Yew client; keep the ticket for third-party/`curl` clients.

3. **`GET /api/events?token=<JWT>`** — accepted only for parity with the reference server's WS
   fallback. **Security tradeoff, stated plainly:** the full-lifetime bearer token lands in
   access logs, `Referer` headers, browser history, and any intermediary's request log; SSE
   connections are long-lived and frequently re-established, multiplying exposure. Gate behind a
   config flag, default off, and log a warning when used.

Rejections mirror the WS close codes but as HTTP: `401` (missing/invalid/expired ticket or token),
`503` + `Retry-After` (at capacity), `500` (server misconfiguration). Once the stream is open, an
expired JWT/ticket-derived session is terminated by closing the response body after emitting a
final `event: session_expired` frame — the client must obtain a fresh ticket before retrying.

### 8.2 Framing

Response headers:

```
Content-Type: text/event-stream; charset=utf-8
Cache-Control: no-store
Connection: keep-alive
X-Accel-Buffering: no          # defeat nginx proxy buffering
```

Every frame:

```
id: 4815162342
event: new_message
data: {"roomId":"room_1749652739650_304e0eaf","msgSerial":1749652900000}

```

(blank line terminates the frame; `data:` is always a single line of compact JSON).

### 8.3 The `id:` field and `Last-Event-ID` resume

`msgSerial` is **per-room**, so it cannot serve alone as a stream cursor for a connection that
multiplexes every room the user is in. PocketSkynet therefore assigns a **global monotonic
`event_seq`** (u64, the JSONL line ordinal, §10) and uses that as `id:`, while carrying the
room-scoped `msgSerial` inside `data:`.

```
id: <event_seq>                            # global, monotonic, resumable cursor
data: {"roomId":"…","msgSerial":<per-room serial>, …}
```

Resume: the browser automatically replays `Last-Event-ID: <event_seq>` on reconnect. The server:

1. Parses it as u64; malformed → treat as "no cursor" (live-tail only).
2. Replays events with `event_seq > cursor` **that the caller is still authorised for** —
   re-filtered at replay time against current membership and current blocks, never trusted from
   the log.
3. Caps replay at `MAX_REPLAY = 1000` events **or** `MAX_REPLAY_AGE = 5 min`. Beyond either, emit

   ```
   id: <current_seq>
   event: resync_required
   data: {"reason":"cursor_too_old","fromSeq":<cursor>,"toSeq":<current_seq>}
   ```

   and let the client do a full `/sync` per room. Bounded replay, unbounded correctness — the
   events are wake-ups (§6), so dropping them only costs a sync.
4. Then switches to live tail.

For a **single-room** stream (`GET /api/events?room=<id>&ticket=…`) the `id:` may instead be the
room's `msgSerial` directly, since it is then unambiguous. Both forms are supported; the
multiplexed form is the default.

### 8.4 Event names and payloads

`event:` names mirror the WebSocket `type` values exactly, so a client shares one deserialiser.

| `event:` | `data:` |
|---|---|
| `new_message` | `{"roomId":"room_…","msgSerial":1749652900000}` |
| `rooms_updated` | `{}` |
| `member_removed` | `{"roomId":"room_…"}` |
| `invitation_received` | `{"roomId":"room_…"}` |
| `typing` | `{"roomId":"room_…","from":"0xabc…"}` (no `id:` — ephemeral, never replayed) |
| `resync_required` | `{"reason":"cursor_too_old","fromSeq":N,"toSeq":M}` |
| `session_expired` | `{"reason":"token_expired"}` (final frame; stream then closes) |

Improvement over the reference: `new_message` carries `msgSerial`, so a client already at or past
that serial can skip the sync entirely. It is still only a hint — content never travels over the
event stream, and the REST `/sync` remains the sole authority for what the user may see.

`typing` frames deliberately omit `id:`, so a `Last-Event-ID` resume never replays stale typing
indicators and the cursor never regresses.

### 8.5 Heartbeats and reconnect

- **Comment frame every 15 s**: a bare `:hb\n\n`. Comments are ignored by every SSE parser but
  keep intermediaries from reaping the connection and let the client detect a dead link.
  15 s is chosen under the common 30–60 s proxy idle window; the reference WS uses 30 s pings,
  but SSE has no protocol-level ping so the margin must be larger.
- **Retry hint**: send `retry: 3000` once at stream start. The browser then reconnects 3 s after a
  drop. On the server side, treat repeated reconnects from the same wallet within a short window
  as backoff pressure and answer `503` + `Retry-After: 30` past the per-wallet cap.
- **Client-side backoff** (Yew, and mandatory for the `fetch()` variant which gets nothing for
  free): `min(1000 * 2^attempts, 30_000)` with **±20 % jitter** — jitter is the one thing the
  reference client lacks, and without it 5000 clients reconnect in lockstep after a deploy.
  Reset on a successfully received first frame, not on socket open.
- **Idle timeout**: 3 min of no *delivered events* does **not** close an SSE stream (unlike WS
  4008) — heartbeats are cheap and SSE reconnects are expensive (full HTTP + ticket round-trip).
  Instead cap total stream lifetime at 30 min, ending with `session_expired`, which forces
  periodic re-authorisation and re-derivation of the membership/block snapshot.

### 8.6 Transport selection

```
prefer WebSocket
  → on handshake failure, or 2 consecutive failed reconnects, or a 4013/503 capacity signal
  → fall back to SSE (ticket-authenticated)
  → on SSE failure  → fall back to 10 s polling
  → periodically (every 5 min) retry the tier above
```

The reference client has no automatic failover (§7); PocketSkynet adds it while keeping the
user-visible manual override (`websocket` | `sse` | `polling`, persisted in `localStorage`).

---

## 9. Rust implementation notes (axum)

### 9.1 The event enum — one type, three encodings

Serde-tagged so the JSON wire form is byte-identical across WS, SSE `data:`, and JSONL.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    NewMessage { room_id: RoomId, #[serde(rename = "msgSerial")] msg_serial: i64 },
    RoomsUpdated,
    MemberRemoved { room_id: RoomId },
    InvitationReceived { room_id: RoomId },
    Typing { room_id: RoomId, from: WalletAddress },
    ResyncRequired { reason: ResyncReason, from_seq: u64, to_seq: u64 },
    SessionExpired { reason: &'static str },
    Pong,
}

impl ServerEvent {
    pub fn name(&self) -> &'static str;      // -> SSE `event:` name / WS `type`
    pub fn is_replayable(&self) -> bool;     // Typing/Pong => false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Ping,
    Typing { room_id: RoomId },
}
```

`WalletAddress` is a newtype that **normalises to lowercase in its parser/`Deserialize`**, making
§5's casing invariant a type-level property rather than a convention.

### 9.2 Routing envelope — who should receive this

The hub broadcasts an envelope; each connection decides locally. This keeps `broadcast` a single
cheap clone-per-receiver and moves filtering to the edge.

```rust
#[derive(Debug, Clone)]
pub struct Envelope {
    pub seq: u64,                       // global monotonic; == JSONL line ordinal
    pub at_ms: i64,
    pub target: Target,
    pub origin: Option<WalletAddress>,  // Some(sender) => recipients may block-filter
    pub event: Arc<ServerEvent>,        // Arc: one allocation for N receivers
}

#[derive(Debug, Clone)]
pub enum Target {
    Room(RoomId),
    User(WalletAddress),
    RoomExcept { room: RoomId, except: WalletAddress },
}
```

`origin` closes the §5 residual leak: a connection drops any envelope whose `origin` is in its
block set, so a blocked user's activity produces no wake-up at all — not merely an empty sync.

### 9.3 Hub

```rust
pub struct Hub {
    tx: broadcast::Sender<Envelope>,       // capacity 1024; lagged receivers => ResyncRequired
    conns: DashMap<ConnId, ConnHandle>,
    by_user: DashMap<WalletAddress, SmallVec<[ConnId; 4]>>,
    by_room: DashMap<RoomId, HashSet<ConnId>>,   // the index the reference server lacks
    total: AtomicUsize,
    seq: AtomicU64,
    log: Arc<JsonlLog>,
}

impl Hub {
    pub fn new(log: Arc<JsonlLog>) -> Arc<Self>;

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope>;

    /// Assigns seq, appends to JSONL (durable before fan-out), then broadcasts.
    pub async fn publish(&self, target: Target, origin: Option<WalletAddress>, ev: ServerEvent)
        -> Result<u64, HubError>;

    pub fn register(&self, conn: ConnHandle) -> Result<ConnId, HubError>;   // enforces the caps
    pub fn unregister(&self, id: ConnId);

    pub async fn refresh_user_rooms(&self, w: &WalletAddress) -> Result<(), HubError>;
    pub async fn refresh_user_blocks(&self, w: &WalletAddress) -> Result<(), HubError>;

    pub fn replay_since(&self, cursor: u64, view: &ConnView, max: usize)
        -> Result<Vec<Envelope>, ReplayError>;
}
```

`publish` writes the JSONL line **before** broadcasting, so the log is a superset of what was
delivered — never the reverse. Ordering of `seq` is guaranteed by taking the atomic and the log
append under the same short critical section.

### 9.4 Per-connection state

```rust
pub struct ConnHandle {
    pub id: ConnId,
    pub wallet: WalletAddress,
    pub kind: ConnKind,                       // Ws | Sse
    pub view: Arc<ArcSwap<ConnView>>,         // lock-free swap on refresh_*
    pub token_exp: Option<i64>,
    pub last_typing_at: AtomicI64,            // 1/s throttle, §4
    pub cancel: CancellationToken,
}

/// The connect-time snapshot: subscriptions + block set. Replaced wholesale, never mutated.
#[derive(Clone)]
pub struct ConnView {
    pub rooms: HashSet<RoomId>,
    pub blocks: HashSet<WalletAddress>,       // union of both directions, §5
}

impl ConnView {
    pub fn accepts(&self, env: &Envelope, me: &WalletAddress) -> bool;
}
```

`ArcSwap<ConnView>` gives the reference server's "overwrite the set on every socket of this user"
semantics without a mutex on the hot delivery path.

### 9.5 Handlers

```rust
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    auth: WsAuth,                       // extractor: Sec-WebSocket-Protocol "fnauth, <jwt>" | ?token=
) -> impl IntoResponse {
    ws.protocols(["fnauth"])            // echo the marker, never the token
      .max_message_size(16 * 1024)      // §4
      .max_write_buffer_size(64 * 1024)
      .on_upgrade(move |socket| ws_conn(socket, app, auth.wallet, auth.exp))
}

async fn sse_handler(
    State(app): State<AppState>,
    auth: SseAuth,                      // extractor: ticket (preferred) | bearer | ?token=
    Query(q): Query<SseQuery>,          // { room: Option<RoomId> }
    last_event_id: Option<TypedHeader<LastEventId>>,
) -> Result<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>, ApiError> {
    // replay_since(cursor) chained ahead of the live BroadcastStream, then
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new().interval(Duration::from_secs(15)).text("hb"),
    ))
}
```

`ws_conn` runs `tokio::select!` over: the socket's inbound half (`ClientMessage` handling: `Ping`
→ `Pong` + reset idle; `Typing` → membership + 1/s throttle + `publish(RoomExcept)`), the
`BroadcastStream` outbound half (filtered by `ConnView::accepts`), a 30 s ping/`missed_pings`
interval, the 180 s idle `tokio::time::Sleep`, and `cancel`. Close codes reuse the §1 table
verbatim — `4001`, `4008`, `4013`, `4500` — so existing native clients need no changes.

`broadcast::error::RecvError::Lagged(n)` is **not** an error to hide: convert it into a
`ResyncRequired` event to that connection. A slow client degrades to "do a full sync", which is
always correct given §6.

### 9.6 Shared state

```rust
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub hub: Arc<Hub>,
    pub log: Arc<JsonlLog>,
    pub jwt: Arc<JwtKeys>,
    pub tickets: Arc<TicketStore>,
    pub cfg: Arc<Config>,
}
```

`AppState` is cheap to clone (all `Arc`), so `State<AppState>` on every handler is free. The hub is
`Arc<Hub>` rather than a channel-driven actor because every mutation is a short `DashMap` write —
an actor would add a hop without removing contention.

---

## 10. JSONL event log

Every realtime event is appended to a line-delimited JSON file **in addition to** the SQLite row
that holds the message itself. SQLite is the queryable state; the JSONL is the ordered, durable,
grep-able history that also backs SSE replay.

### 10.1 Layout

Under the data directory — `~/.pocketskynet` by default, `POCKETSKYNET_PATH`
or `--data-dir` to relocate:

```
<data-dir>/events/
  events-2026-07-27.jsonl        # daily rotation, UTC
  events-2026-07-26.jsonl
  events.seq                     # last committed seq, for crash recovery
```

Opened `O_APPEND`, one writer task, `BufWriter` flushed per publish, `fdatasync` batched every
100 ms or 64 lines (whichever first). `O_APPEND` + a single writer makes torn interleaving
impossible; a torn *final* line after a crash is discarded on load (parse failure at EOF only).

### 10.2 Record shape

One JSON object per line, no pretty-printing, keys in a stable order, `\n`-terminated:

```json
{"seq":4815162342,"ts":"2026-07-27T09:14:22.881Z","at_ms":1785316462881,"kind":"realtime","target":{"t":"room","room_id":"room_1749652739650_304e0eaf"},"origin":"0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22","event":{"type":"new_message","room_id":"room_1749652739650_304e0eaf","msgSerial":1749652900000},"fanout":7}
```

| Field | Type | Meaning |
|---|---|---|
| `seq` | u64 | **Global monotonic**, gapless, never reused. The SSE `id:`. Line ordinal within the logical log. |
| `ts` | RFC 3339 UTC | human/grep-friendly |
| `at_ms` | i64 | epoch millis; the machine-comparable form |
| `kind` | `"realtime"` \| `"audit"` \| `"system"` | log multiplexing; SSE replay reads `realtime` only |
| `target` | tagged object | `{"t":"room","room_id":…}` \| `{"t":"user","wallet":…}` \| `{"t":"room_except","room_id":…,"except":…}` |
| `origin` | string \| null | originating wallet, lowercase — drives replay-time block filtering |
| `event` | tagged object | the **exact** `ServerEvent` JSON put on the wire |
| `fanout` | u32 | number of connections it was delivered to; 0 = nobody listening (ops signal) |

Rotation writes a `{"seq":…,"kind":"system","event":{"type":"log_rotated","from":"events-2026-07-26.jsonl"}}`
record as the first line of the new file, so `seq` continuity is verifiable across files.

### 10.3 Relationship to `msgSerial`

Two distinct counters — conflating them is the single easiest mistake here:

| | `msgSerial` | `seq` |
|---|---|---|
| Scope | **per room** | **global** |
| Source | messages table, millisecond-derived, monotonic within a room | JSONL append counter |
| Meaning | position in a room's message history | position in the server's event stream |
| Used by | `GET /api/rooms/:id/sync?since=` | SSE `id:` / `Last-Event-ID` |
| Gaps | expected (blocked senders' rows are filtered out of a viewer's `/sync`) | never — gapless by construction |
| Comparable across rooms | no | yes |

Invariant: for every `new_message` event, `event.msgSerial` equals the `msgSerial` of the SQLite
row committed immediately before the log append; the transaction commits first, so any event in
the log has durable state behind it. The reverse does not hold — a crash between commit and append
loses an event but never a message, which is exactly the tolerable direction given §6 (the client
resyncs by `msgSerial` and recovers).

Recovery on boot: read `events.seq`, scan the tail of the newest file to the last parseable line,
take `max(seq)+1` as the next value, and emit a `system`/`log_recovered` record. SSE replay
requests whose cursor precedes the oldest retained file get `resync_required` (§8.3) rather than a
partial replay.

### 10.4 Why both stores

- SQLite answers "what is the state of room X" — indexed, joinable, the REST path's source.
- JSONL answers "what happened, in what order, globally" — append-only, cheap to ship to a log
  pipeline, trivially diffable in tests, and the only structure that can serve `Last-Event-ID`
  replay without a second table and its own retention policy.
- Divergence is detectable: replaying `kind:"realtime"` `new_message` records must reproduce the
  per-room `max(msgSerial)` held in SQLite. That equality is a cheap integration-test assertion
  and a production consistency check.
