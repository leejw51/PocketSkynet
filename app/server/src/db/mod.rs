//! SQLite access.
//!
//! # Why a hand-rolled pool
//!
//! `rusqlite` is synchronous, and calling it directly from an async handler
//! would park a tokio worker thread on a disk write. Every query in this
//! server therefore runs inside [`tokio::task::spawn_blocking`], reached
//! through [`Db::call`]. Handlers stay `async` and never see a `Connection`
//! escape into a future.
//!
//! The pool itself is deliberately small and dumb: a `Vec<Connection>` behind
//! a mutex, gated by a semaphore so at most [`POOL_SIZE`] blocking tasks are
//! ever in flight. That is enough because SQLite in WAL mode admits many
//! concurrent readers but exactly one writer — a larger pool would only queue
//! writers deeper inside SQLite's own lock rather than in ours, where waiting
//! is cheap and observable. Pulling in `r2d2` or `deadpool` would add a
//! dependency to reimplement these thirty lines.
//!
//! Three pragmas matter and are set on **every** connection, because SQLite
//! scopes them per-connection, not per-database:
//!
//! * `journal_mode = WAL` — readers never block the writer, which is what
//!   makes a single-writer pool acceptable.
//! * `foreign_keys = ON` — off by default in SQLite. The schema leans on
//!   `ON DELETE CASCADE` for the transactional room delete, and without this
//!   pragma those clauses are silently inert.
//! * `busy_timeout` — turns `SQLITE_BUSY` from an error the caller must retry
//!   into a bounded wait, which is the behaviour every call site wants.

pub mod admin;
pub mod files;
pub mod keys;
pub mod mentions;
pub mod messages;
pub mod models;
pub mod operators;
pub mod rooms;
pub mod shouts;
pub mod sites;
pub mod storage;
pub mod uploads;
pub mod users;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};
use tokio::sync::Semaphore;

use crate::error::{ApiError, ApiResult};

/// Concurrent blocking database tasks. See the module docs for why it is small.
pub const POOL_SIZE: usize = 4;

/// How long a statement waits for a competing writer before giving up.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// The schema, replayed at every startup. Every statement is idempotent.
const SCHEMA: &str = include_str!("schema.sql");

/// Bumped only for diagnostics — the schema is applied by replay, not by a
/// migration ladder, so this records what a database was last opened by.
const SCHEMA_VERSION: i64 = 3;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("opening database at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("applying schema: {0}")]
    Schema(#[source] rusqlite::Error),
    #[error("creating scratch directory {0}: {1}")]
    Scratch(PathBuf, #[source] std::io::Error),
}

struct Inner {
    path: String,
    /// Connections not currently checked out. Guarded by a `std::sync::Mutex`
    /// rather than a tokio one because it is only ever held for a `pop`/`push`.
    idle: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
    /// Set only for throwaway databases, which delete themselves when the last
    /// handle goes away.
    scratch: Option<PathBuf>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(dir) = &self.scratch {
            // Close every pooled connection before removing the files, or
            // Windows would refuse and the directory would leak.
            self.idle.lock().map(|mut idle| idle.clear()).ok();
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A cloneable handle to the database. Cloning shares the pool.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.inner.path)
            .finish()
    }
}

impl Db {
    /// Open (or create) the database at `path` and apply the schema.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::open_uri(&path.to_string_lossy(), None)
    }

    /// Open a throwaway database in a private temporary directory, removed
    /// when the last handle drops.
    ///
    /// Deliberately **not** `:memory:` with `cache=shared`. That combination
    /// looks like the obvious choice for tests, but shared-cache SQLite uses
    /// table-level locking and answers a second writer with `SQLITE_LOCKED`,
    /// which `busy_timeout` does not retry — so any test with concurrent
    /// writers fails for a reason that has nothing to do with the code under
    /// test. A real file in WAL mode exercises exactly the production paths.
    pub fn open_temp() -> Result<Self, DbError> {
        let mut tag = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut tag);
        let dir = std::env::temp_dir().join(format!("ps-db-{}", hex::encode(tag)));
        std::fs::create_dir_all(&dir).map_err(|e| DbError::Scratch(dir.clone(), e))?;
        let path = dir.join("pocketskynet.db");
        Self::open_uri(&path.to_string_lossy(), Some(dir))
    }

    fn open_uri(uri: &str, scratch: Option<PathBuf>) -> Result<Self, DbError> {
        let mut conns = Vec::with_capacity(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            conns.push(open_connection(uri)?);
        }

        // Schema first, on one connection, before anything can read.
        migrate(&conns[0]).map_err(DbError::Schema)?;

        // Index any messages that predate the search feature. One anti-join
        // on an up-to-date database; a real pass only on first upgrade.
        match crate::search::store::backfill(&conns[0]) {
            Ok(0) => {}
            Ok(n) => tracing::info!("search index backfilled {n} messages"),
            // Search being behind is degraded, not fatal — the messenger
            // must still come up.
            Err(e) => tracing::warn!("search backfill failed: {e}"),
        }

        Ok(Self {
            inner: Arc::new(Inner {
                path: uri.to_owned(),
                idle: Mutex::new(conns),
                permits: Arc::new(Semaphore::new(POOL_SIZE)),
                scratch,
            }),
        })
    }

    /// Run `f` against a pooled connection on the blocking pool.
    ///
    /// The closure owns its inputs (`'static`) because it crosses a thread
    /// boundary; that is why storage functions take owned ids rather than
    /// references.
    pub async fn call<F, T>(&self, f: F) -> ApiResult<T>
    where
        F: FnOnce(&mut Connection) -> ApiResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self
            .inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("database pool closed")))?;

        let mut conn = self.checkout()?;
        let inner = self.inner.clone();

        let joined = tokio::task::spawn_blocking(move || {
            let out = f(&mut conn);
            (conn, out)
        })
        .await;

        drop(permit);

        match joined {
            Ok((conn, out)) => {
                // Return the connection only on a clean exit. A panicking task
                // may have left an open transaction behind, and re-using such
                // a connection would corrupt the next caller's work.
                inner.idle.lock().expect("pool mutex").push(conn);
                out
            }
            Err(e) => Err(ApiError::Internal(
                anyhow::Error::new(e).context("database task panicked"),
            )),
        }
    }

    /// Run `f` synchronously. Only for tests and startup, where blocking the
    /// caller is correct; handlers must use [`Db::call`].
    pub fn call_blocking<F, T>(&self, f: F) -> ApiResult<T>
    where
        F: FnOnce(&mut Connection) -> ApiResult<T>,
    {
        let mut conn = self.checkout()?;
        let out = f(&mut conn);
        self.inner.idle.lock().expect("pool mutex").push(conn);
        out
    }

    fn checkout(&self) -> ApiResult<Connection> {
        if let Some(conn) = self.inner.idle.lock().expect("pool mutex").pop() {
            return Ok(conn);
        }
        // The semaphore bounds concurrency, so an empty pool means a previous
        // task panicked and dropped its connection. Replace it rather than
        // failing the request.
        open_connection(&self.inner.path)
            .map_err(|e| ApiError::Internal(anyhow::Error::new(e).context("reopening connection")))
    }
}

fn open_connection(uri: &str) -> Result<Connection, DbError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;

    let open = |uri: &str| -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open_with_flags(uri, flags)?;
        // An in-memory database refuses WAL; that is not an error worth
        // failing startup over, so the result is queried, not asserted.
        let _: Result<String, _> = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0));
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS.into()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // NORMAL is the documented companion to WAL: durable across process
        // crashes, and only at risk from a power loss in the last commit.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    };

    open(uri).map_err(|source| DbError::Open {
        path: PathBuf::from(uri),
        source,
    })
}

/// Apply the schema. Idempotent by construction: every statement is
/// `CREATE ... IF NOT EXISTS`, so this runs unconditionally at each startup
/// and needs no version ladder to consult.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;

    // Additive upgrades. `CREATE TABLE IF NOT EXISTS` cannot retrofit a
    // column onto a table that predates it, so each late column is probed
    // for and added — still idempotent, still no version ladder.
    //
    // Every one of these is nullable or defaulted, which is what lets an old
    // row mean the right thing without a backfill: a room from before DMs
    // existed is a channel, and a message from before threads existed is not
    // a reply.
    add_column(conn, "users", "profile_image", "TEXT")?;
    add_column(conn, "rooms", "kind", "TEXT NOT NULL DEFAULT 'channel'")?;
    add_column(conn, "rooms", "dm_key", "TEXT")?;
    add_column(conn, "messages", "parent_message_id", "TEXT")?;

    // Indexes over the columns just added. These cannot live in `schema.sql`:
    // on a database that predates the column, the batch above would reach the
    // index before the ALTER below could add what it indexes, and a failing
    // statement aborts the *whole* batch — taking every table declared after
    // it with it. Creating them here, after the retrofit, is the only ordering
    // that works on a fresh database and an upgraded one alike.
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_rooms_dm_key
             ON rooms (dm_key) WHERE dm_key IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_messages_parent
             ON messages (parent_message_id, message_timestamp)
             WHERE parent_message_id IS NOT NULL;",
    )?;

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Add one column if the table does not already have it.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, and running the `ALTER` blind and
/// swallowing the error would swallow real ones too — a typo'd type, a
/// disk-full — so the probe is the honest form.
fn add_column(conn: &Connection, table: &str, column: &str, decl: &str) -> rusqlite::Result<()> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |r| r.get(0),
    )?;
    if present == 0 {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

/// Milliseconds since the Unix epoch — the storage form of every timestamp.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
pub fn test_db() -> Db {
    Db::open_temp().expect("scratch database")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_application_is_idempotent() {
        let db = test_db();
        db.call_blocking(|conn| {
            // Replaying the schema must not fail on an already-populated
            // database — startup does exactly this on every boot.
            migrate(conn).unwrap();
            migrate(conn).unwrap();
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_database_from_before_profile_image_gains_the_column() {
        // A deployment that predates the avatar feature has a users table
        // without `profile_image`; `CREATE TABLE IF NOT EXISTS` will skip it,
        // so the retrofit branch in `migrate` is what upgrades it.
        let mut tag = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut tag);
        let dir = std::env::temp_dir().join(format!("ps-mig-{}", hex::encode(tag)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pocketskynet.db");

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (
                     wallet_address TEXT PRIMARY KEY,
                     username       TEXT NOT NULL,
                     public_key     TEXT,
                     public_key_sig TEXT,
                     created_at     INTEGER NOT NULL,
                     updated_at     INTEGER NOT NULL
                 ) STRICT;
                 INSERT INTO users VALUES ('0xaa', 'alice', NULL, NULL, 1, 1);",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        db.call_blocking(|conn| {
            let user = users::get_user(conn, "0xaa").unwrap().unwrap();
            assert_eq!(user.username, "alice");
            assert_eq!(user.profile_image, None, "existing rows read as unset");
            Ok(())
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_database_from_before_threads_and_dms_upgrades_completely() {
        // The regression this pins down is subtle and total: `schema.sql` is
        // one `execute_batch`, and a statement that fails aborts the rest of
        // it. So an index over `rooms.dm_key` declared in that file would,
        // on a database whose rooms table predates the column, fail — and
        // take every table declared *after* it (messages, mentions, files,
        // payments…) with it. A server would then come up against a database
        // missing half its schema.
        let mut tag = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut tag);
        let dir = std::env::temp_dir().join(format!("ps-mig2-{}", hex::encode(tag)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pocketskynet.db");

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE rooms (
                     id                   TEXT PRIMARY KEY,
                     name                 TEXT NOT NULL,
                     description          TEXT,
                     current_key_version  INTEGER NOT NULL DEFAULT 1,
                     key_rotation_pending INTEGER NOT NULL DEFAULT 0,
                     created_at           INTEGER NOT NULL
                 ) STRICT;
                 CREATE TABLE messages (
                     id                TEXT PRIMARY KEY,
                     room_id           TEXT NOT NULL,
                     sender_address    TEXT NOT NULL,
                     content           TEXT NOT NULL,
                     msg_hash          TEXT NOT NULL,
                     message_timestamp INTEGER NOT NULL,
                     msg_type          TEXT NOT NULL DEFAULT 'add',
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
                 INSERT INTO rooms VALUES ('room_old', 'General', NULL, 1, 0, 1);",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        db.call_blocking(|conn| {
            // The room predates DMs, so it reads as the channel it always was.
            let room = rooms::get_room(conn, "room_old").unwrap().unwrap();
            assert_eq!(room.kind, "channel");

            // Tables declared after the retrofitted columns in `schema.sql`
            // are present, which is what the aborted-batch bug destroyed.
            for table in ["message_mentions", "suspended_users", "payments", "files"] {
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        rusqlite::params![table],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(n, 1, "{table} missing after upgrade");
            }

            // And the indexes that could only be built after the ALTERs.
            for index in ["idx_rooms_dm_key", "idx_messages_parent"] {
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                        rusqlite::params![index],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(n, 1, "{index} missing after upgrade");
            }
            Ok(())
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_keys_are_enforced_on_every_pooled_connection() {
        let db = test_db();
        for _ in 0..POOL_SIZE * 2 {
            let on: i64 = db
                .call_blocking(|conn| {
                    Ok(conn
                        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                        .unwrap())
                })
                .unwrap();
            assert_eq!(on, 1, "cascade deletes are silently inert without this");
        }
    }

    #[test]
    fn cascade_removes_every_room_scoped_row() {
        let db = test_db();
        db.call_blocking(|conn| {
            conn.execute(
                "INSERT INTO rooms (id, name, created_at) VALUES ('room_abcdefghij', 'x', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO room_members (room_id, user_address, joined_at)
                 VALUES ('room_abcdefghij', '0xaa', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO room_invitations (room_id, invited_address, invited_by, created_at)
                 VALUES ('room_abcdefghij', '0xbb', '0xaa', 1)",
                [],
            )
            .unwrap();

            conn.execute("DELETE FROM rooms WHERE id = 'room_abcdefghij'", [])
                .unwrap();

            let members: i64 = conn
                .query_row("SELECT COUNT(*) FROM room_members", [], |r| r.get(0))
                .unwrap();
            let invites: i64 = conn
                .query_row("SELECT COUNT(*) FROM room_invitations", [], |r| r.get(0))
                .unwrap();
            // §15 #11: the reference left invitations behind forever.
            assert_eq!((members, invites), (0, 0));
            Ok(())
        })
        .unwrap();
    }

    #[tokio::test]
    async fn call_returns_connections_to_the_pool() {
        let db = test_db();
        // More iterations than the pool holds: a leak would deadlock or start
        // opening fresh connections, and the in-memory database would then
        // appear empty.
        for i in 0..POOL_SIZE * 4 {
            let n: i64 = db
                .call(move |conn| {
                    conn.execute(
                        "INSERT INTO rooms (id, name, created_at) VALUES (?1, 'x', 1)",
                        rusqlite::params![format!("room_pool_{i:04}")],
                    )
                    .unwrap();
                    Ok(conn
                        .query_row("SELECT COUNT(*) FROM rooms", [], |r| r.get(0))
                        .unwrap())
                })
                .await
                .unwrap();
            assert_eq!(n, i as i64 + 1);
        }
    }

    #[tokio::test]
    async fn a_panicking_task_does_not_poison_the_pool() {
        let db = test_db();
        let panicked = db
            .call(|_| -> ApiResult<()> { panic!("boom") })
            .await
            .is_err();
        assert!(panicked, "a panic must surface as an error, not a hang");

        // The pool must still serve requests afterwards.
        let n: i64 = db
            .call(|conn| Ok(conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap()))
            .await
            .unwrap();
        assert_eq!(n, 1);
    }
}
