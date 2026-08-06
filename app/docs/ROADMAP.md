# PocketSkynet — Advanced Communication for Humans and AI

*Date: 2026-08-06. Direction: not a Slack clone — a communication platform
where humans and AI agents are equal participants. Supersedes the stale parts
of [PARITY.md](PARITY.md).*

**Step 1 of §6 has shipped, end to end.** Mentions, threads and DMs exist as
protocol, storage *and* interface, with a server-admin role beside them. §0
records what that means concretely.

## 0. What shipped in step 1

Server-side, all tested (`server/` unit tests + `tests/e2e`, which drives a
real server over HTTP and a real browser):

- **Direct messages** — `POST /api/rooms/dm`, idempotent on the member set, so
  the conversation between two people is one room however either of them opens
  it. Group DMs up to 9. A DM refuses rename / invite / kick / leave / promote,
  because none of those verbs applies to it, and every member is an admin so it
  has no owner.
- **Threads** — `parentMessageId` on send, always flattened to the thread root;
  `GET /api/messages/{id}/thread`; replies excluded from the channel view with
  `replyCount` / `lastReplyAt` on the parent instead.
- **Mentions** — `message_mentions` rows resolved to *wallet addresses* at write
  time (so a rename cannot break them), from the client's declared list unioned
  with server-side `@token` parsing of plaintext. `GET /api/mentions` is the
  inbox; `mentionCount` per room is the badge. Read state is derived from the
  room's read pointer, so there is no second thing to keep in step.
- **Server admin** — a wallet in `VITE_FRUITNATION_ADMIN`. Configuration, not a
  table: see `.env.example` for why. Can suspend (which revokes *existing*
  tokens), remove somebody from every room, delete any room, and administer any
  room. Cannot read a conversation they are not in — no endpoint returns
  message content.

Three gaps from §3 and §5 closed on the way:

- **History protection** — purging a room is now admin-only, not any member.
- **Site deletion** — now the owner or a server admin, not any signed-in user.
- **Message ordering** — the same-millisecond tiebreak was `id`, which is
  `msg_{millis}_{uuid}`, so it ordered by a *random UUID*. Now `msg_serial`.
  The identical bug in the files listing is fixed too.

Client-side, all driven in a real browser by `tests/e2e/browser.spec.js`:

- **DMs** are listed under their own heading, titled after the other member
  (derived per viewer, so a rename cannot stale it), with every channel-only
  control hidden. A "New message" picker starts one, including the
  note-to-self.
- **Threads** render inline under their parent behind a "N replies" chip, on a
  rail — not in a side panel, because on a phone a side panel *is* the screen,
  which turns "glance at the replies" into "leave the conversation". Opening
  one costs no request: `/sync` already delivered the replies, so the client
  folds them locally and a thread opens instantly and offline. The composer
  shows an unmissable chip naming what it is replying to, and clears it on
  send — a sticky thread is how people post an unrelated remark into somebody
  else's conversation.
- **Mentions** highlight in the bubble, with a *filled* chip when they name
  you, because that is the one thing somebody scans a busy room for. The
  composer has an `@` autocomplete (arrows, Enter, Tab, Escape) that records
  the address rather than the string, which is what makes a name with a space
  in it work at all. There is an inbox behind the `@` in the top bar and a
  per-room badge beside the unread count.
- **The admin console** appears only for wallets the server names as admins,
  and echoes back the list it parsed from `VITE_FRUITNATION_ADMIN` — the only
  way to catch a typo there, whose sole other symptom is a colleague who
  mysteriously has no powers.

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

## 1. Core communication primitives ~~(blockers)~~ — **shipped**

These were needed whether the participant is human or AI. All three now exist
in the protocol and the storage layer; see §0 for what the client renders yet.

- ~~**DMs / group DMs**~~ — `POST /api/rooms/dm`, keyed on the member set.
- ~~**Threads**~~ — `parent_message_id`, flattened to one level.
- ~~**@mentions**~~ — resolved to addresses at write time, with an inbox.

Mentions being addresses rather than strings is what makes the assistant idea
work later: `@researcher summarize this thread` resolves to a participant, and
a participant can be an agent holding a wallet exactly as a person does. The
attention rule in §2 has the mechanism it needed.

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

- ~~**Session revocation / member removal**~~ — **done.** Suspension is checked
  at the point a token is presented, so it revokes credentials already issued;
  `DELETE /api/admin/users/{addr}` removes somebody from every room at once and
  flags each for re-keying.
- ~~**Server owner role**~~ — **done.** `VITE_FRUITNATION_ADMIN` (§6.14 of
  API.md). Deliberately configuration rather than a table, because an admin
  table is a table whose first row has to come from somewhere.
- **Invite links / codes** — still open. Invites are by wallet address only. A
  link or QR that onboards a new member is table stakes.
- ~~**History protection**~~ — **done.** Purging a room is admin-only.
- **Presence** — still nothing. Now the most-missed thing on this list.

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
- ~~Any signed-in user can delete any published site.~~ **Fixed** — the owner
  or a server admin. Publishing costs a real on-chain payment, and "anyone can
  delete what you paid for" is not a property a paid feature can have.
- ~~No `msg_serial` tiebreak in `GET /api/messages`.~~ **Fixed**, and it was
  worse than "non-deterministic": the tiebreak was `m.id`, which looks stable
  and is `msg_{millis}_{uuid}` — so same-millisecond messages were ordered by a
  random UUID. A three-reply thread posted in one burst came back shuffled. The
  identical bug in `db/files.rs` is fixed too.
- Wallet-backup JSON can be written but not loaded back at sign-in.
- Solana / Cardano appear in the wallet registry but cannot sign.
- Zero browser/DOM test coverage (`wasm-bindgen-test` declared, unused).
- Stale docs: `app/docs/PARITY.md`, `app/.env.example:60-64`,
  `app/web/src/components/login.rs:4-8`.

---

## 6. Suggested build order

1. ~~**Mentions + threads + DMs**~~ — **shipped** (§0), server and client.
2. ~~**Server owner role + session revocation**~~ — **shipped** (§0).
3. **Invite links** — the one §3 item step 1 did not touch, and the thing
   between "a team could use this" and "a team could join this".
4. **Webhooks** — external events into rooms; the minimum integration
   surface.
5. **Presence** — online/away for humans.

The wallet identity, E2EE, on-chain payment rails, and shared knowledge base
are the platform's edge — every step above builds on them rather than
replacing them with Slack's model.
