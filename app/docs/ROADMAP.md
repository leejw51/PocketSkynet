# PocketSkynet — Advanced Communication for Humans and AI

*Date: 2026-08-06. Direction: not a Slack clone — a communication platform
where humans and AI agents are equal participants. Supersedes the stale parts
of [PARITY.md](PARITY.md).*

**Steps 1, 2 and 5 of §6 have shipped, end to end.** Mentions, threads and DMs
exist as protocol, storage *and* interface, with a server-admin role beside
them, and presence now says who is actually there. §0 records what that means
concretely.

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

## 0a. What shipped in step 5 — presence

Online / away / offline for everybody you share a room with, on the server
(`server/src/hub.rs`, `routes/presence.rs`) and on screen.

- **Derived, never stored.** No table, no column, no migration. The truth is
  the set of live connections the hub already holds plus how recently each
  showed a sign of life, so nothing survives a restart — which is the point. A
  durable presence record is a log of when each person was at their computer,
  and "is she there right now?" does not need one.
- **Three states, and no more.** "Busy" and "in a meeting" are statuses people
  set once and never clear; a status nobody maintains is worse than none. A
  wallet's status is the *maximum* over its devices, so the phone idling in
  somebody's pocket never contradicts the laptop they are typing on.
- **What the server cannot see, the client says.** A tab going to the
  background is invisible from the server side, and an idle timer alone gets
  both ends of it wrong — somebody reading a long thread would go away, and
  somebody who shut the lid would stay. `visibilitychange` drives a `presence`
  frame over WebSocket, or `PUT /api/presence` on the tiers with no upstream
  channel, where the same call doubles as their heartbeat. Without it a silent
  SSE stream would age into a false *away* and a polling client would never
  appear at all. A protocol-level pong deliberately does **not** count as
  activity: browsers answer those whether or not the page is running.
- **It does not cross a block, in either direction** — the rule typing already
  followed, for the same reason. Presence is an activity oracle, and a
  one-directional filter would make it answer "did they block me?". Sharing a
  room is the whole of what entitles you to know; a server admin gets no
  exemption.
- **One fact, one event.** Going online concerns everyone you share *any* room
  with, so it publishes against the whole set at once (`Target::Rooms`) and each
  connection tests that set against its own current subscriptions. Somebody in
  three shared rooms hears it once, and a member who joined a second ago is
  authorised for the very next one — there is no cached peer set to go stale.
- **The snapshot is the authority.** Presence events are transient and never
  replayed, so a disconnection leaves a *hole* rather than a stale value, and
  nothing re-announces a status that has not changed. `GET /api/presence` fills
  it, and the client calls it whenever a transport proves healthy.

On screen: a filled dot for online, a **ring** for away, and nothing at all for
offline. The difference is shape rather than a second colour, because these
land on generated portraits in every hue in the palette — and because colour is
never the only signal (DESIGN.md §17), the roster spells the word out beside the
name and the room list carries it for screen readers. Offline draws nothing:
it is where most people are most of the time, and a badge on every absent
colleague is a screen of noise.

The dot appears where it changes a decision — the members roster, one-to-one DM
rows, and the DM header, which is what tells you whether the message you are
about to type gets read now or tomorrow morning. Channel rows have none: a room
is not somewhere anybody *is*, and a dot there would either invent a status for
a group or quietly pick one member to speak for it.

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

§7 narrows the first and third: a rendering-only subset, and notification
tiers that stay self-hosted. The exclusions still stand against full
markdown and third-party push.

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
- ~~**Invite links / codes**~~ — **done.** Admins mint per-room links with
  expiry, an optional use limit, and immediate revocation; the token is a
  bearer capability stored only as a hash (API.md §6.7a). The landing page
  carries a newcomer from the link through create-wallet into the room, and
  joining an encrypted room flags it for re-keying, mirroring leave/kick. §7 M1.
- ~~**History protection**~~ — **done.** Purging a room is admin-only.
- ~~**Presence**~~ — **done.** Online / away / offline, derived from live
  connections rather than stored, scoped to shared rooms and filtered by blocks
  both ways. §0a.

---

## 4. Platform & scale

- **Integration surface** — incoming webhooks **shipped** (§7 M4, API.md
  §17): CI, GitHub, monitoring → room, per-room token, plaintext rooms only.
  Still open: outgoing webhooks, slash commands, a public API contract.
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
3. ~~**Invite links**~~ — **shipped** (§7 M1), server and client. The thing
   between "a team could use this" and "a team could join this".
4. **Webhooks** — external events into rooms; the minimum integration
   surface. Broken into tasks in §7 M4.
5. ~~**Presence**~~ — **shipped** (§0a), server and client. Taken out of order
   because it cost no schema: the hub already knew who was connected, and every
   piece of the authorisation it needed — shared rooms, and blocks in both
   directions — was already written for typing.

The wallet identity, E2EE, on-chain payment rails, and shared knowledge base
are the platform's edge — every step above builds on them rather than
replacing them with Slack's model.

---

## 7. Team-messenger TODO

*Added 2026-08-07. Scope: ~100 people, one on-premises server — the daily
messenger for a team, not Slack parity. One node already covers this scale.
In priority order; M1 and M2 decide whether people stay after day one.*

**M1 — Invite links** (no onboarding funnel exists today)
- [x] Server: create / list / revoke; signed token, expiry, per room —
      **shipped** (API.md §6.7a): `inv_` + 32 CSPRNG bytes, stored only as a
      SHA-256, expiry and an optional use budget enforced by one conditional
      UPDATE at redeem, revocation immediate
- [x] Server: redeem token → membership, re-key flag for encrypted rooms —
      **shipped**: joining a room that holds wraps sets `keyRotationPending`,
      the same flag leave/kick set from the other side
- [x] Client: share dialog (link + QR), revocation list — **shipped**: one
      admin dialog mints, shows URL + QR (inline SVG), and revokes
- [x] Client: landing flow — link → create wallet → signed in → in the room —
      **shipped**: `/invite/{token}` parks the token across the sign-in
      journey and redeems it the moment a session exists

**M2 — Notifications** (all tiers on-premises; mute ships with it)
- [ ] Tab title / favicon badge + mention sound
- [ ] Browser Notification API for backgrounded tabs
- [ ] Tauri native notifications on desktop
- [ ] Per-room mute / mentions-only / DND hours

**M3 — Writing ergonomics** (render-side only; wire stays plain text)
- [x] Mention another user by `@…` — **shipped** (§0): composer autocomplete,
      highlight in the bubble, per-room badge, inbox behind the `@` icon
- [ ] Autocomplete inserts `@nickname(0x1234..5678)` — search as you type,
      then replace the typed handle with name *plus* short address, so two
      people with the same nickname cannot be mistaken for each other and
      the text itself says who was meant, even after a rename
- [ ] Code spans + fenced code blocks
- [ ] Pinned messages per room
- [ ] Per-room persistent drafts

**M4 — Operating it** (one server, somebody has to run it)
- [x] Incoming webhooks: per-room token, POST → message, plaintext rooms only
      — **shipped**: `POST /api/webhooks/{token}` (the token is the auth, own
      rate budget), admin create/list/revoke in the room menu, derived
      `0x00000000…` sender identity with a webhook badge, refused for E2EE
      rooms at create *and* post time (API.md §17)
- [ ] Backup / restore doc (SQLite + `data/`, one page)

**M5 — Reliability debt (§5)**
- [ ] WS→SSE→polling fallback actually engages (`realtime.rs:123`) — this is
      what corporate networks that block WebSockets hit
- [ ] Verify anchoring tx hash server-side
- [ ] Wallet-backup JSON loads back at sign-in

Stays excluded: calls / screen share, full markdown, server-side agents,
federation, multi-node, Slack history import.
