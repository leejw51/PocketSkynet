# Integration-suite findings

Divergences between the running server and the specifications in `docs/`, found
by the end-to-end suite in `server/tests/`.

Where §15 of `docs/API.md` lists a reference-implementation quirk **and** a
recommendation, the recommendation is the contract: PocketSkynet is a port that
fixes those, so the suite asserts the recommended behaviour. All 22 of those
recommendations that the suite covers are met.

**There are currently no open findings.** The suite is fully green with nothing
ignored. The two findings raised during development are recorded below with how
they were resolved, because both are decisions someone may want to revisit.

---

## Resolved 1 — rotation-coverage rejection lacks a `missing[]` array

**Was:** `POST /api/rooms/:roomId/rotate-key` folded a coverage failure into the
generic `Validation failed` envelope, leaving the uncovered addresses embedded
in an English sentence inside `errors[]`. §10.3 rule 5 tells clients to refetch
and retry automatically after a rotation race, which would have meant parsing
addresses out of prose.

**Resolution: not a real divergence.** The finding was recorded against an
earlier build. `server/src/routes/keys.rs` emits the specified shape:

```json
{"message":"Rotation must include a key for every current member",
 "missing":["0x8d915ebd…"]}
```

Now asserted by
`keys.rs::a_coverage_failure_lists_the_missing_members_in_a_missing_array`,
which passes.

---

## Resolved 2 — the WebSocket `?token=` fallback is gated

**Was:** `docs/REALTIME.md` §1 and `docs/API.md` §12.1 presented
`ws://host/ws?token=<JWT>` as an unconditional fallback "retained for native
CLIs", while §8.1 gated only the *SSE* query token. `StreamAuth` applied the SSE
gate to both, so a CLI written against §1 got `401 Invalid token` — a message
pointing at the credential rather than at a server flag.

**Resolution: the code was right, the docs were wrong, and the message was
unhelpful.** A full-lifetime bearer token in a URL is recorded by access logs,
proxy logs, `Referer` headers and browser history; that exposure is a property
of putting a credential in a URL, not of the transport carrying it, so gating
SSE but not WebSocket was arbitrary. The gate stays.

Two things changed instead:

1. `docs/REALTIME.md` §1 and `docs/API.md` §12.1 now document the gate and name
   `--sse-token-query`.
2. The rejection now says what to do:
   `Query-string tokens are disabled on this server; use the
   Sec-WebSocket-Protocol handshake or an SSE ticket, or start the server with
   --sse-token-query`.

Asserted by
`realtime.rs::a_gated_token_query_handshake_names_the_flag_rather_than_blaming_the_token`
and `realtime.rs::the_websocket_token_query_fallback_is_off_by_default`.

---

## Notes that are not findings

- **`POST /api/users/block` with a non-string `address`.** §6.4.3 check 1 names
  one message for "missing or not a string", but the wrong-type half is caught
  by body deserialization and returns the `Validation failed` envelope instead
  of the plain `Wallet address is required`. Both are `400`, the missing-field
  half matches the spec exactly, and no legitimate client sends a non-string, so
  `users.rs` asserts the exact message for the missing case and status-only for
  the wrong-type case.
- **SSE heartbeat spelling.** `REALTIME.md` §8.5 writes the comment frame as
  `:hb`; axum emits `: hb`. Both are valid SSE comments and every parser ignores
  them identically. The test accepts either.
- ~~**`GET /messages` has no tiebreak on `messageTimestamp`.**~~ **Fixed.** The
  tiebreak was `m.id`, which looks stable and is not: an id is
  `msg_{millis}_{uuid}`, so messages sharing a millisecond were ordered by a
  random UUID. Now `msg_serial`, the room's own monotonic counter. Threads
  surfaced this — a three-reply thread posted in one burst came back shuffled
  under parallel test load, which the same ordering had been doing to the
  channel view all along, just less visibly.
- **On-chain verification of a published anchor is not implemented** — the
  `FN_RPC_URL` path of §6.10.6 step 4. `server/src/routes/messages.rs` documents
  this as deliberate, and the format, recipient, ownership and already-anchored
  checks all behave as specified, so the suite tests those and does not test
  transaction verification.
