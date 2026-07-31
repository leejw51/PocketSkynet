-- PocketSkynet schema.
--
-- Transcribed from docs/API.md §4, with the fixes §15 asks for:
--
--   * Unique indexes on blocked_users, hidden_rooms, room_members, room_admins
--     and room_keys. The reference had none, so its `ON CONFLICT DO NOTHING`
--     clauses were no-ops and repeated calls inserted duplicate rows that then
--     surfaced as duplicated entries in list responses (§15 #4).
--   * `room_serials`, a durable per-room counter. The reference kept the last
--     issued serial in a process-local map, so two replicas sharing a database
--     could issue the same serial in the same millisecond — and a client
--     paging on `msg_serial > since` would then skip one of the two messages
--     forever (§15 #2). Allocation happens inside the writing transaction.
--   * ON DELETE CASCADE from every room-scoped table. Deleting a room in the
--     reference was seven unsynchronised statements that skipped
--     room_invitations entirely, leaving orphans (§15 #11). One `DELETE FROM
--     rooms` inside a transaction now removes everything atomically.
--
-- Timestamps are epoch milliseconds (INTEGER), formatted to ISO-8601 at the
-- serialisation boundary. Storing them as text would make ordering depend on
-- the format string, and SQLite has no native date type to lean on.
--
-- Booleans are 0/1 INTEGERs, which is what SQLite stores anyway.
--
-- Every statement is idempotent, so this file is replayed verbatim at each
-- startup instead of being tracked by a migration table.

CREATE TABLE IF NOT EXISTS users (
    wallet_address  TEXT    PRIMARY KEY,
    username        TEXT    NOT NULL,
    public_key      TEXT,
    public_key_sig  TEXT,
    -- The chosen avatar: `preset:<name>` for a gallery portrait, or an
    -- `/api/images/<sha256>.<ext>` URL for an uploaded / AI-generated one.
    -- NULL means the client derives one from the address hash as before.
    -- Databases created before this column exist get it retrofitted in
    -- `db::migrate` — `CREATE TABLE IF NOT EXISTS` cannot add a column.
    profile_image   TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
) STRICT;

-- Per-account salt for the E2EE key-derivation message. Served only to its
-- owner: a public salt would let any page reconstruct the derivation message
-- and phish the signature that *is* the user's encryption private key.
CREATE TABLE IF NOT EXISTS encryption_salts (
    wallet_address  TEXT    PRIMARY KEY,
    salt            TEXT    NOT NULL,
    created_at      INTEGER NOT NULL
) STRICT;

-- Login challenges. Single-use: consumed with a DELETE ... RETURNING before
-- any validation runs, so a failed attempt burns the challenge and a replayed
-- signature has nothing to replay against.
CREATE TABLE IF NOT EXISTS auth_challenges (
    id              TEXT    PRIMARY KEY,
    wallet_address  TEXT    NOT NULL,
    nonce           TEXT    NOT NULL,
    message         TEXT    NOT NULL,
    expires_at      INTEGER NOT NULL,
    created_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_auth_challenges_expiry
    ON auth_challenges (expires_at);

CREATE TABLE IF NOT EXISTS rooms (
    id                   TEXT    PRIMARY KEY,
    name                 TEXT    NOT NULL,
    description          TEXT,
    current_key_version  INTEGER NOT NULL DEFAULT 1,
    key_rotation_pending INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL
) STRICT;

-- The next msgSerial a room will hand out. Bumped in the same transaction as
-- the row that consumes it, which is the whole point of the table.
CREATE TABLE IF NOT EXISTS room_serials (
    room_id     TEXT    PRIMARY KEY REFERENCES rooms (id) ON DELETE CASCADE,
    next_serial INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS room_admins (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id        TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    wallet_address TEXT    NOT NULL,
    created_at     INTEGER NOT NULL,
    UNIQUE (room_id, wallet_address)
) STRICT;

CREATE TABLE IF NOT EXISTS room_members (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id      TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    user_address TEXT    NOT NULL,
    joined_at    INTEGER NOT NULL,
    UNIQUE (room_id, user_address)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_room_members_user
    ON room_members (user_address);

CREATE TABLE IF NOT EXISTS room_invitations (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id         TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    invited_address TEXT    NOT NULL,
    invited_by      TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    UNIQUE (room_id, invited_address)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_room_invitations_invitee
    ON room_invitations (invited_address, created_at DESC);

-- One wrapped room key per (room, user, epoch). A member accumulates one row
-- per epoch they can read, which is what preserves their access to history
-- across rotations.
CREATE TABLE IF NOT EXISTS room_keys (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id                 TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    user_address            TEXT    NOT NULL,
    encrypted_symmetric_key TEXT    NOT NULL,
    ephemeral_public_key    TEXT    NOT NULL,
    encryption_iv           TEXT    NOT NULL,
    hmac                    TEXT    NOT NULL,
    enc_ver                 INTEGER NOT NULL DEFAULT 1,
    key_version             INTEGER NOT NULL DEFAULT 1,
    created_at              INTEGER NOT NULL,
    UNIQUE (room_id, user_address, key_version)
) STRICT;

CREATE TABLE IF NOT EXISTS blocked_users (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    blocker_address  TEXT    NOT NULL,
    blocked_address  TEXT    NOT NULL,
    created_at       INTEGER NOT NULL,
    UNIQUE (blocker_address, blocked_address)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_blocked_users_blocked
    ON blocked_users (blocked_address);

CREATE TABLE IF NOT EXISTS hidden_rooms (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_address TEXT    NOT NULL,
    room_id      TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    created_at   INTEGER NOT NULL,
    UNIQUE (user_address, room_id)
) STRICT;

-- Reactions are rows here too, not a separate table: an add/remove is an
-- append-only message with target_message_id + emoticon_code set, so
-- reactions flow through /sync with the same cursor as everything else.
CREATE TABLE IF NOT EXISTS messages (
    id                TEXT    PRIMARY KEY,
    room_id           TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    sender_address    TEXT    NOT NULL,
    content           TEXT    NOT NULL,
    msg_hash          TEXT    NOT NULL,
    message_timestamp INTEGER NOT NULL,
    msg_type          TEXT    NOT NULL DEFAULT 'add',
    msg_serial        INTEGER NOT NULL DEFAULT 0,
    is_deleted        INTEGER NOT NULL DEFAULT 0,
    edited_at         INTEGER,
    created_at        INTEGER NOT NULL,
    is_encrypted      INTEGER NOT NULL DEFAULT 0,
    iv                TEXT,
    hmac              TEXT,
    enc_ver           INTEGER NOT NULL DEFAULT 1,
    key_version       INTEGER NOT NULL DEFAULT 1,
    tx_hash           TEXT,
    target_message_id TEXT,
    emoticon_code     TEXT
) STRICT;

-- The sync cursor index: every /sync query is (room_id, msg_serial > ?).
CREATE INDEX IF NOT EXISTS idx_messages_room_serial
    ON messages (room_id, msg_serial);

-- Display ordering and the /messages timestamp pagination.
CREATE INDEX IF NOT EXISTS idx_messages_room_ts
    ON messages (room_id, message_timestamp DESC);

-- Reaction aggregation replays every event for one target message.
CREATE INDEX IF NOT EXISTS idx_messages_target
    ON messages (target_message_id, msg_serial);

CREATE TABLE IF NOT EXISTS room_reads (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id          TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    user_address     TEXT    NOT NULL,
    last_read_serial INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    UNIQUE (room_id, user_address)
) STRICT;

-- ------------------------------------------------------------------ search --
-- The knowledge index (docs/SEARCH.md). One row per searchable document:
-- a plaintext chat message, a taught knowledge note, or a room profile.
-- Encrypted messages are NEVER rows here — the server cannot read them, and
-- must not learn them through a side table.
--
-- `embedding` is the local hashed-feature vector (search/embed.rs), f32
-- little-endian. Semantic similarity is a brute-force cosine scan, which at
-- a personal server's message count is faster than any index would pay for.

CREATE TABLE IF NOT EXISTS search_docs (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind      TEXT    NOT NULL,           -- 'message' | 'knowledge' | 'room'
    ref_id    TEXT    NOT NULL,           -- message id / note id / room id
    room_id   TEXT,                       -- provenance + membership scoping
    sender    TEXT,                       -- author wallet address
    ts        INTEGER NOT NULL,
    text      TEXT    NOT NULL,
    tags      TEXT    NOT NULL DEFAULT '',
    embedding BLOB    NOT NULL,
    UNIQUE (kind, ref_id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_search_docs_room ON search_docs (room_id);

-- BM25 side of the hybrid ranking. External-content: the text lives once, in
-- search_docs; the triggers below are the contract that keeps the two in step.
CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
    text, tags,
    content='search_docs', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS search_docs_ai AFTER INSERT ON search_docs BEGIN
    INSERT INTO search_fts (rowid, text, tags) VALUES (new.id, new.text, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS search_docs_ad AFTER DELETE ON search_docs BEGIN
    INSERT INTO search_fts (search_fts, rowid, text, tags)
        VALUES ('delete', old.id, old.text, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS search_docs_au AFTER UPDATE ON search_docs BEGIN
    INSERT INTO search_fts (search_fts, rowid, text, tags)
        VALUES ('delete', old.id, old.text, old.tags);
    INSERT INTO search_fts (rowid, text, tags) VALUES (new.id, new.text, new.tags);
END;

-- One row per (tag, doc): exact hashtag filtering and the browse counts.
CREATE TABLE IF NOT EXISTS hashtags (
    tag    TEXT    NOT NULL,
    doc_id INTEGER NOT NULL REFERENCES search_docs (id) ON DELETE CASCADE,
    UNIQUE (tag, doc_id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_hashtags_tag ON hashtags (tag);

-- Taught knowledge (docs/SEARCH.md "teach"). Server-global on purpose: the
-- product is a self-hosted shared brain, so anything taught is searchable by
-- every logged-in user; only the author may edit or delete it.
CREATE TABLE IF NOT EXISTS knowledge_notes (
    id                TEXT    PRIMARY KEY,
    owner_address     TEXT    NOT NULL,
    content           TEXT    NOT NULL,
    room_id           TEXT,               -- optional provenance
    source_message_id TEXT,               -- when taught from a chat message
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_knowledge_owner ON knowledge_notes (owner_address);

-- Attachments. The bytes live on the filesystem under `data/files/`, named by
-- their own SHA-256; this table stores the *path component* and the metadata,
-- never the content. A blob column would put megabytes into every backup of a
-- database whose other rows are all a few hundred bytes, and would defeat the
-- write-then-rename that makes an interrupted upload leave nothing behind.
--
-- `stored_name` is the whole filesystem coupling: `data/files/{stored_name}`.
-- It is content-addressed, so two people uploading the same bytes share one
-- file on disk and get two rows here — which is why the unique key is the row
-- id and *not* the hash. Deleting a row therefore must not delete the file;
-- see `db/files.rs::delete` for why that is deliberate rather than a leak.
--
-- `caption` is the searchable half: it carries the hashtags (docs/SEARCH.md
-- §1), which is what makes an attachment findable at all. The filename alone
-- is a poor index — "scan_0142.pdf" says nothing — so the tags are the point.
CREATE TABLE IF NOT EXISTS files (
    id           TEXT    PRIMARY KEY,
    room_id      TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    uploader     TEXT    NOT NULL,
    filename     TEXT    NOT NULL,      -- as the uploader named it, for display
    stored_name  TEXT    NOT NULL,      -- {sha256}.{ext} under data/files/
    mime         TEXT    NOT NULL,
    size_bytes   INTEGER NOT NULL,
    caption      TEXT    NOT NULL,      -- may be empty; carries the #hashtags
    created_at   INTEGER NOT NULL
) STRICT;

-- Newest-first per room is the only read path the drawer has.
CREATE INDEX IF NOT EXISTS idx_files_room ON files (room_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_files_uploader ON files (uploader);

-- Paid features (docs/API.md §16: Shout and web publishing). Every paid
-- action references exactly one on-chain transfer to the operator's
-- FruitNation wallet (`VITE_FRUITNATION_WALLET`) — that payment is both the
-- business model and the spam gate. This table is what makes a transaction
-- hash single-use: the PRIMARY KEY turns "replay the same receipt for a
-- second shout" into a constraint violation instead of free money.
CREATE TABLE IF NOT EXISTS payments (
    tx_hash       TEXT    PRIMARY KEY,      -- 0x + 64 hex, lowercased
    payer_address TEXT    NOT NULL,
    amount_wei    TEXT    NOT NULL,         -- decimal string; wei exceeds i64
    purpose       TEXT    NOT NULL,         -- 'shout' | 'site'
    created_at    INTEGER NOT NULL
) STRICT;

-- Paid broadcasts. A shout is deliberately NOT a message: it belongs to no
-- room, is never encrypted, and burns out after at most a minute. Rows are
-- kept after expiry as the operator's revenue ledger; the active set is
-- always read through `expires_at`.
CREATE TABLE IF NOT EXISTS shouts (
    id             TEXT    PRIMARY KEY,
    sender_address TEXT    NOT NULL,
    text           TEXT    NOT NULL,
    tx_hash        TEXT    NOT NULL,
    amount_wei     TEXT    NOT NULL,
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_shouts_expires ON shouts (expires_at DESC);

-- Hosted sites (docs/API.md §16.2). One CRO buys hosting: the uploaded HTML
-- (or unpacked zip) lives under `data/sites/{id}/` and is served publicly at
-- `/sites/{id}/`. The row records provenance and the payment; the search
-- index (kind 'site') is what makes a site findable. Deletion is open to ANY
-- signed-in user by design — this is a shared LAN wall, and anything pinned
-- to it can be torn down by whoever it annoys.
CREATE TABLE IF NOT EXISTS published_sites (
    id            TEXT    PRIMARY KEY,     -- [0-9a-f-] uuid; path component
    owner_address TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    tx_hash       TEXT    NOT NULL,
    amount_wei    TEXT    NOT NULL,
    size_bytes    INTEGER NOT NULL,        -- unpacked bytes on disk
    file_count    INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_sites_created ON published_sites (created_at DESC);
