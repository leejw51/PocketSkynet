# PocketSkynet — Advanced Communication for Humans and AI

*Date: 2026-08-05. Direction: not a Slack clone — a communication platform
where humans and AI agents are equal participants. Supersedes the stale parts
of [PARITY.md](PARITY.md).*

## Where it stands today

PocketSkynet is a self-hosted, wallet-authenticated messenger (Rust/axum +
SQLite server, Yew/WASM web client, Tauri desktop) with E2EE rooms, reactions,
hybrid FTS5+embedding search, file attachments, a Cronos wallet, and
bring-your-own-key AI assistants (Grok, OpenAI, Anthropic, Gemini).

Its foundations are already unusual and worth keeping:

- **Wallet identity** — no passwords, no accounts; identity works the same
  for a human or an agent. An AI agent can hold a wallet, sign a challenge,
  and log in exactly like a person.
- **E2EE rooms** with key epochs and rotation.
- **AI in the client** — draft, reply-in-context, image/video generation,
  RAG over a shared knowledge base, and a tool-calling on-chain agent
  (AI Banker).
- **On-chain anchoring and payments** — messages can be anchored, features
  can be paid for, all verified over RPC.

## Excluded by design (non-goals)

These were considered and deliberately left out — they are design decisions,
not gaps:

- **Markdown / code-block rendering** — messages stay plain text by design.
- **Server-side agent accounts / agent API** — AI stays in the client,
  bring-your-own-key; no headless agent runtime on the server.
- **Push notifications** (Web Push / APNs / FCM) — in-app unread badges are
  the notification model.

---

## 1. Core communication primitives (blockers)

These are needed whether the participant is human or AI:

- **DMs / group DMs** — no 1:1 concept; the workaround is manually creating
  a 2-person room.
- **Threads** — no `parent_message_id` in the schema or protocol. Threads
  matter double here: they are the natural container for an AI's multi-step
  reply without flooding the room.
- **@mentions** — no parsing, highlight, or mention notifications. Mentions
  are also the natural way to *address an assistant* (`@researcher summarize
  this thread`).

---

## 2. AI in the room (the differentiator)

AI lives in the client (BYO key, browser-side) and stays there. Deepening it:

- **Tool use / actions in rooms** — AI Banker proves the tool-loop pattern
  client-side; generalize it: assistants that can search the knowledge base,
  fetch room history, call external tools (MCP is the obvious protocol),
  and post results back.
- **Shared memory** — the knowledge notes + hashtag + RAG system is the seed
  of an org brain. Missing: per-room scoping and provenance ("taught from
  message X").
- **E2EE and AI** — the Reply/Ask flows already ship decrypted content to a
  provider opt-in per press; keep that consent model explicit as AI surface
  area grows.
- **Attention rules** — if multiple members run assistants in one room,
  mention-gating keeps them from talking over each other.

---

## 3. Running it as a team platform

Lighter than enterprise compliance, but still needed for a real team:

- **Session revocation / member removal** — JWTs are valid until `exp` with
  no revocation list; there is no way to remove someone from the server.
  Room-level kick exists; server-level does not.
- **Server owner role** — no admin above per-room admin. Someone must be
  able to remove users, delete rooms, and set server policy.
- **Invite links / codes** — invites are by wallet address only. A link or
  QR that onboards a new member is table stakes.
- **History protection** — any room member can purge an entire room
  (`DELETE /api/rooms/{id}/messages` is not admin-gated).
- **Presence** — none exists today.

---

## 4. Platform & scale

- **Integration surface** — no webhooks (in or out), no slash commands, no
  public API contract. Incoming webhooks (CI, GitHub, monitoring → room)
  are the minimum company glue.
- **Single-node only** — in-process hub, SQLite, in-process rate limiter.
  Fine for a team-sized deployment; acknowledged in PARITY.md.
- **E2EE vs. search/AI tension** — encrypted messages are excluded from
  search by design. Make the trade-off explicit at room creation
  (the E2EE toggle already exists there).

---

## 5. Smaller gaps and partial implementations

- WS→SSE→polling fallback is implemented and tested but never engages —
  call site hardcodes failure count `0` (`app/web/src/realtime.rs:123`,
  `app/web/src/app.rs:171`).
- `POST /api/messages/{id}/publish` (anchoring) does not verify the tx hash
  server-side; paid features do.
- Any signed-in user can delete any published site.
- No `msg_serial` tiebreak in `GET /api/messages` — same-millisecond
  ordering is non-deterministic (`app/server/tests/FINDINGS.md`).
- Wallet-backup JSON can be written but not loaded back at sign-in.
- Solana / Cardano appear in the wallet registry but cannot sign.
- Zero browser/DOM test coverage (`wasm-bindgen-test` declared, unused).
- Stale docs: `app/docs/PARITY.md`, `app/.env.example:60-64`,
  `app/web/src/components/login.rs:4-8`.

---

## 6. Suggested build order

1. **Mentions + threads + DMs** — communication primitives both humans and
   assistants need; mentions double as the assistant-activation mechanism.
2. **Server owner role + session revocation** — remove members from the
   server, not just from rooms.
3. **Webhooks** — external events into rooms; the minimum integration
   surface.
4. **Presence** — online/away for humans.

The wallet identity, E2EE, on-chain payment rails, and shared knowledge base
are the platform's edge — every step above builds on them rather than
replacing them with Slack's model.
