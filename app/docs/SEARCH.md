# Search & Knowledge

The core promise: **anything written in a chatroom is findable later, and anything worth
keeping can be taught.** Retrieval runs entirely on the server's SQLite — no cloud, no
model downloads, no network beyond the LAN. Cloud AI enters only at the very end, on the
client, from retrieved passages, with the user's explicit per-ask consent.

This document is the contract. `server/src/search/` implements it; the web client's
Knowledge page consumes it.

---

## 1. Architecture

```
 write path                                read path
 ──────────                                ─────────
 message posted ──┐                        query ─► #tags split off (hard filter)
 note taught ─────┤                              ├─► FTS5 MATCH  ── BM25 rank ──┐
                  ▼                              └─► local embed ── cosine rank ─┤
        search_docs (text, tags,                                                 ▼
        embedding) + search_fts                              reciprocal-rank fusion
        (FTS5) + hashtags                                        (k = 60) ─► results
```

* **BM25** — SQLite FTS5 (`unicode61 remove_diacritics 2`), external-content table kept
  in step by triggers. Exact-word relevance.
* **Semantic** — 384-dim hashed-feature embeddings (`search/embed.rs`): words, word
  bigrams, `^`-padded character trigrams, CJK character uni/bigrams, FNV-1a hashed with
  sign, L2-normalised. This is what survives typos (`kubernets` → `kubernetes`) and CJK
  compounds (`김치찌개` matches `김치`) that BM25 misses. Deterministic forever: the hash
  function is pinned in-source, so an index never silently disagrees with its binary.
* **Fusion** — both rankings merge by reciprocal rank (RRF, k=60), so a document strong
  on either side surfaces without either side's scores needing calibration.
* **Hashtags** — `#tag` at a word boundary (letters/digits/`_`/`-`, at least one letter;
  URL fragments excluded; lowercased). In a query, tags are *hard filters*, not terms;
  a tag-only query browses newest-first.

## 2. What is indexed — and what never is

| Source | Indexed? |
| --- | --- |
| Plaintext chat message (`msg_type=add`) | yes, at write time, same transaction |
| **Encrypted message** | **never** — the server cannot read it, and no derived table may learn what `messages` does not say |
| Knowledge note (teach) | yes |
| Reactions, tombstones, system events | no |

Edits re-index (an edit to encrypted also *un*-indexes); deletes, room purges and room
deletions forget their documents in the same transaction. On startup, a one-shot
anti-join backfills any plaintext messages that predate the feature.

## 3. Visibility

* **Messages**: current room membership, minus blocked senders — the same scope as every
  other read path. Leaving a room removes its history from your search.
* **Knowledge**: server-global on purpose. A self-hosted server is a shared brain;
  anything taught is meant to be found by every logged-in user. Only the author may
  delete a note. (Do not teach secrets — the UI says so.)

## 4. HTTP API

All endpoints require `Authorization: Bearer`, rate-limited like the rest of `/api`.

| Endpoint | Meaning |
| --- | --- |
| `GET /api/search?q=…&kind=message\|knowledge&limit=n` | Hybrid search. Empty/tag-only `q` browses newest-first. → `{results: [{kind, refId, roomId, sender, timestamp, text, tags, score}]}` |
| `GET /api/search/tags?limit=n` | Visible tag cloud → `{tags: [{tag, count}]}` |
| `POST /api/knowledge` `{content, roomId?, sourceMessageId?}` | Teach. Content 1–5000 chars; hashtags extracted from it. → the note |
| `GET /api/knowledge?owner=me&limit=n` | Notes newest-first → `{notes}` |
| `DELETE /api/knowledge/{id}` | Forget. 403 for non-authors, 404 unknown |

`score` orders results within one response only; it is not comparable across queries.

## 5. Client: the Knowledge page

A first-class pane (⚡ in the rail next to the room list), two modes matching the two
verbs:

* **Search** — one box searches everything visible. Results carry provenance (room,
  sender, time) and jump to the room on click. Hashtags render as chips everywhere and
  clicking one anywhere (including inside a chat bubble) opens Knowledge filtered by it.
* **Teach** — a composer that saves a knowledge note. "Teach" also appears in a
  message's `⋮` menu, pre-filling the note from that message with provenance attached.

### The quick bar (main page)

One glowing field above the room list — the index's front door. Its chip label *is* the
contract: **AI Search** when this device holds a text-provider key (the question and the
top retrieved passages will go to that provider and come back as an answer, no further
prompt), plain **Search** otherwise (retrieval only, nothing leaves the LAN). Enter lands
on the Knowledge page with the retrieval run and, in the AI case, the answer generating.

### Cloud AI answers ("Ask")

When at least one AI provider key exists in this device's assistant settings, a search
can be *escalated*: the client takes the top retrieved passages and asks the configured
text provider to answer from them (RAG — retrieval here, generation there). Rules:

1. **Off by default.** A plain search never leaves the device/server.
2. **Explicit consent per ask.** The button names the provider ("Ask Grok from these
   results"); first use in a session shows what will be sent — the query and the
   retrieved passages, nothing else.
3. **No key, no button.** Without a configured provider the feature is invisible, and
   search remains fully functional — the product works with zero cloud.
4. Encrypted-room content can only reach a provider if the *client* includes locally
   decrypted passages it already holds; the server can never supply them. v1 sends only
   server-retrieved (plaintext) passages.

## 6. Non-goals (v1)

* No embedding model downloads or ONNX runtimes on the server.
* No indexing of encrypted content, even client-side-decrypted, into the server index.
* No cross-server federation; the index is one server's corpus.
* No stemming/lemmatisation beyond what unicode61 + trigrams give — the semantic side
  is the recall net.
