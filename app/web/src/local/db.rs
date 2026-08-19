//! The local backend's storage: a thin wrapper over IndexedDB.
//!
//! Deliberately dumb. Every value is a JSON **string** and every key is a
//! string the caller composed (`logic::message_key` and friends) — no key
//! paths, no indexes, no `JsValue` object graphs. Ordered iteration falls out
//! of IndexedDB's string key order, which the zero-padded serial in the
//! message keys is designed for. What the rows *mean* lives in `router`/`logic`.
//!
//! One database per account (`ps-local-<address>`), so two wallets on one
//! browser never see each other's rows and "erase local data" can drop one
//! account's world whole.
//!
//! The one piece of cleverness: [`Db::commit_with_serial`] allocates the next
//! `msgSerial` and applies the caller's writes **in a single transaction**, so
//! serials stay strictly increasing even with two tabs open.
//!
//! On the host this module is a stub that errors: `cargo test` compiles the
//! crate, and the pure logic is tested without a browser; the real storage
//! runs under `make test-wasm`.

/// Store names. One place, because a typo'd store name in IndexedDB is a
/// runtime `NotFoundError` rather than a compile error.
pub const META: &str = "meta";
pub const MESSAGES: &str = "messages";
pub const MSGID: &str = "msgid";
pub const WRAPS: &str = "wraps";
pub const KNOWLEDGE: &str = "knowledge";
pub const PASSWORDS: &str = "passwords";
pub const MEDIA: &str = "media";

const STORES: [&str; 7] = [META, MESSAGES, MSGID, WRAPS, KNOWLEDGE, PASSWORDS, MEDIA];

/// One write in a [`Db::commit_with_serial`] batch.
pub enum Op {
    Put {
        store: &'static str,
        key: String,
        value: String,
    },
    Delete {
        store: &'static str,
        key: String,
    },
}

#[cfg(target_arch = "wasm32")]
pub use imp::*;

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::{Op, META, STORES};

    use std::cell::RefCell;
    use std::rc::Rc;

    use idb::{DatabaseEvent, Factory, KeyRange, ObjectStoreParams, TransactionMode};
    use wasm_bindgen::JsValue;

    // The open database, cached for the tab's life. Keyed by owner so an
    // account switch reopens rather than crossing streams.
    thread_local! {
        static OPEN: RefCell<Option<(String, Rc<Db>)>> = const { RefCell::new(None) };
    }

    pub struct Db {
        inner: idb::Database,
    }

    fn db_name(owner: &str) -> String {
        format!("ps-local-{owner}")
    }

    fn err<E: std::fmt::Debug>(e: E) -> String {
        format!("Local storage error: {e:?}")
    }

    fn key(k: &str) -> JsValue {
        JsValue::from_str(k)
    }

    /// Open (or return the cached handle to) this owner's database.
    pub async fn open(owner: &str) -> Result<Rc<Db>, String> {
        let cached = OPEN.with(|o| {
            o.borrow()
                .as_ref()
                .filter(|(name, _)| name == owner)
                .map(|(_, db)| db.clone())
        });
        if let Some(db) = cached {
            return Ok(db);
        }
        let factory = Factory::new().map_err(err)?;
        let mut request = factory.open(&db_name(owner), Some(1)).map_err(err)?;
        request.on_upgrade_needed(|event| {
            if let Ok(database) = event.database() {
                for store in STORES {
                    // Out-of-line string keys throughout; params stay default.
                    let _ = database.create_object_store(store, ObjectStoreParams::new());
                }
            }
        });
        let database = request.await.map_err(err)?;
        let db = Rc::new(Db { inner: database });
        OPEN.with(|o| *o.borrow_mut() = Some((owner.to_owned(), db.clone())));
        Ok(db)
    }

    /// Drop the cached handle (account switch, sign-out to a different mode).
    pub fn close() {
        OPEN.with(|o| {
            if let Some((_, db)) = o.borrow_mut().take() {
                db.inner.close();
            }
        });
    }

    /// Destroy this owner's database entirely — the "Erase local data" path.
    pub async fn delete_database(owner: &str) -> Result<(), String> {
        close();
        let factory = Factory::new().map_err(err)?;
        factory
            .delete(&db_name(owner))
            .map_err(err)?
            .await
            .map_err(err)
    }

    impl Db {
        pub async fn get(&self, store: &str, k: &str) -> Result<Option<String>, String> {
            let tx = self
                .inner
                .transaction(&[store], TransactionMode::ReadOnly)
                .map_err(err)?;
            let s = tx.object_store(store).map_err(err)?;
            let value = s.get(key(k)).map_err(err)?.await.map_err(err)?;
            tx.await.map_err(err)?;
            Ok(value.and_then(|v| v.as_string()))
        }

        pub async fn put(&self, store: &str, k: &str, value: &str) -> Result<(), String> {
            let tx = self
                .inner
                .transaction(&[store], TransactionMode::ReadWrite)
                .map_err(err)?;
            let s = tx.object_store(store).map_err(err)?;
            s.put(&JsValue::from_str(value), Some(&key(k)))
                .map_err(err)?
                .await
                .map_err(err)?;
            tx.commit().map_err(err)?.await.map_err(err)?;
            Ok(())
        }

        pub async fn delete(&self, store: &str, k: &str) -> Result<(), String> {
            let tx = self
                .inner
                .transaction(&[store], TransactionMode::ReadWrite)
                .map_err(err)?;
            let s = tx.object_store(store).map_err(err)?;
            s.delete(key(k)).map_err(err)?.await.map_err(err)?;
            tx.commit().map_err(err)?.await.map_err(err)?;
            Ok(())
        }

        /// Every value whose key is in `[from, to]`, ascending by key. With
        /// `last = Some(n)`, only the final `n` of that range — the shape a
        /// newest-first history page wants.
        pub async fn get_range(
            &self,
            store: &str,
            from: &str,
            to: &str,
            last: Option<usize>,
        ) -> Result<Vec<String>, String> {
            let range =
                KeyRange::bound(&key(from), &key(to), Some(false), Some(false)).map_err(err)?;
            let tx = self
                .inner
                .transaction(&[store], TransactionMode::ReadOnly)
                .map_err(err)?;
            let s = tx.object_store(store).map_err(err)?;
            let values = s
                .get_all(Some(range.into()), None)
                .map_err(err)?
                .await
                .map_err(err)?;
            tx.await.map_err(err)?;
            let mut out: Vec<String> = values.into_iter().filter_map(|v| v.as_string()).collect();
            if let Some(n) = last {
                if out.len() > n {
                    out.drain(..out.len() - n);
                }
            }
            Ok(out)
        }

        /// Every value in a store, ascending by key.
        pub async fn get_all(&self, store: &str) -> Result<Vec<String>, String> {
            let tx = self
                .inner
                .transaction(&[store], TransactionMode::ReadOnly)
                .map_err(err)?;
            let s = tx.object_store(store).map_err(err)?;
            let values = s.get_all(None, None).map_err(err)?.await.map_err(err)?;
            tx.await.map_err(err)?;
            Ok(values.into_iter().filter_map(|v| v.as_string()).collect())
        }

        /// Delete every key in `[from, to]`.
        pub async fn delete_range(&self, store: &str, from: &str, to: &str) -> Result<(), String> {
            let range =
                KeyRange::bound(&key(from), &key(to), Some(false), Some(false)).map_err(err)?;
            let tx = self
                .inner
                .transaction(&[store], TransactionMode::ReadWrite)
                .map_err(err)?;
            let s = tx.object_store(store).map_err(err)?;
            s.delete(idb::Query::from(range))
                .map_err(err)?
                .await
                .map_err(err)?;
            tx.commit().map_err(err)?.await.map_err(err)?;
            Ok(())
        }

        /// Allocate the next message serial and apply `build`'s writes, all in
        /// one transaction. The counter is global (not per room), mirroring
        /// the server; what matters to `/sync` is strict monotonicity, which
        /// the single transaction guarantees even across tabs.
        pub async fn commit_with_serial(
            &self,
            build: impl FnOnce(i64) -> Vec<Op>,
        ) -> Result<i64, String> {
            let tx = self
                .inner
                .transaction(&STORES, TransactionMode::ReadWrite)
                .map_err(err)?;
            let meta = tx.object_store(META).map_err(err)?;
            let current: i64 = meta
                .get(key("serial"))
                .map_err(err)?
                .await
                .map_err(err)?
                .and_then(|v| v.as_string())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let next = current + 1;
            meta.put(&JsValue::from_str(&next.to_string()), Some(&key("serial")))
                .map_err(err)?
                .await
                .map_err(err)?;
            for op in build(next) {
                match op {
                    Op::Put {
                        store,
                        key: k,
                        value,
                    } => {
                        tx.object_store(store)
                            .map_err(err)?
                            .put(&JsValue::from_str(&value), Some(&key(&k)))
                            .map_err(err)?
                            .await
                            .map_err(err)?;
                    }
                    Op::Delete { store, key: k } => {
                        tx.object_store(store)
                            .map_err(err)?
                            .delete(key(&k))
                            .map_err(err)?
                            .await
                            .map_err(err)?;
                    }
                }
            }
            tx.commit().map_err(err)?.await.map_err(err)?;
            Ok(next)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use stub::*;

/// Host stubs so `cargo test` compiles the crate. Nothing here is reachable
/// from a test that matters — the pure logic lives in `logic`, and the real
/// storage is exercised by the wasm test suite.
#[cfg(not(target_arch = "wasm32"))]
mod stub {
    #![allow(dead_code, unused_variables)]

    use super::Op;
    use std::rc::Rc;

    pub struct Db;

    const UNAVAILABLE: &str = "Local storage is only available in a browser";

    pub async fn open(owner: &str) -> Result<Rc<Db>, String> {
        Err(UNAVAILABLE.into())
    }

    pub fn close() {}

    pub async fn delete_database(owner: &str) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }

    impl Db {
        pub async fn get(&self, store: &str, k: &str) -> Result<Option<String>, String> {
            Err(UNAVAILABLE.into())
        }
        pub async fn put(&self, store: &str, k: &str, value: &str) -> Result<(), String> {
            Err(UNAVAILABLE.into())
        }
        pub async fn delete(&self, store: &str, k: &str) -> Result<(), String> {
            Err(UNAVAILABLE.into())
        }
        pub async fn get_range(
            &self,
            store: &str,
            from: &str,
            to: &str,
            last: Option<usize>,
        ) -> Result<Vec<String>, String> {
            Err(UNAVAILABLE.into())
        }
        pub async fn get_all(&self, store: &str) -> Result<Vec<String>, String> {
            Err(UNAVAILABLE.into())
        }
        pub async fn delete_range(&self, store: &str, from: &str, to: &str) -> Result<(), String> {
            Err(UNAVAILABLE.into())
        }
        pub async fn commit_with_serial(
            &self,
            build: impl FnOnce(i64) -> Vec<Op>,
        ) -> Result<i64, String> {
            Err(UNAVAILABLE.into())
        }
    }
}
