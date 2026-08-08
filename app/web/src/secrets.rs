//! Skynet Password, client side — the policy over `core::secrets`.
//!
//! # What this buys, and what it costs
//!
//! A password manager inside a messenger is a strange place to keep the keys to
//! your life, and it should be justified rather than assumed. The justification
//! is narrow: this app already asks you to hold a wallet, already derives an
//! encryption identity from it, and already keeps that identity out of storage.
//! A secret store built on that material inherits all three properties for
//! free — it syncs to a second device with no export step, the server that
//! hosts it cannot read it, and there is no new password to lose. Building the
//! same thing on a fresh passphrase would mean adding a key-stretching
//! dependency to a wasm bundle, and giving people one more string whose loss is
//! unrecoverable.
//!
//! What it costs is stated in [`pocketskynet_core::secrets`] and is worth
//! repeating at the boundary people actually read:
//!
//! * **Losing the wallet loses the entries.** There is no recovery, no reset
//!   link and no operator who can help. That is the same bargain the rest of
//!   the product makes, and it is a worse one here, because a room you lose
//!   access to still exists for its other members and a password you lose
//!   access to is simply gone.
//! * **A script on this origin reads everything.** The vault key lives in the
//!   session for the lifetime of the tab, so an XSS holds it. Nothing
//!   client-side survives that; this module does not pretend to.
//! * **The server sees the shape.** How many entries, when each changed, and
//!   roughly how long each field is.
//!
//! # Decrypted values are never cached, listed, or persisted
//!
//! The threat model this layer *does* enforce is lifetime minimization, not
//! zeroization (guaranteed wiping is theatre on the wasm heap, which is a
//! JS-visible `ArrayBuffer` the GC moves at will). The rule is: decrypt a
//! **value** late, use it, drop it — never hold it in state, in a cache, or in
//! any storage backend.
//!
//! Concretely, and this is what the two entry points here are shaped for:
//!
//! * The list decrypts only each entry's **label** ([`Vault::open_label`]),
//!   because the label is all the list shows. Labels are what
//!   [`components::passwords`](crate::components::passwords) memoises — never the
//!   values.
//! * A **value** is opened ([`Vault::open_value`]) only at the instant the user
//!   reveals or copies that one entry, and the plaintext is dropped as soon as
//!   the reveal is collapsed or the copy completes. It is never stashed beside
//!   the label, and there is deliberately no `open` that returns both halves of
//!   a whole list at once — that method used to exist and was exactly the
//!   resident-plaintext hazard this section rules out.
//!
//! The residue this cannot remove, stated plainly rather than hidden: a value
//! is briefly in the heap while it is on screen (revealed) or in flight to the
//! clipboard, and the plaintext sits in an input field while it is being typed
//! or edited. Those are unavoidable — you cannot show or edit a secret without
//! its plaintext existing somewhere for that moment. Everything else is
//! minimized. Nothing decrypted, and not the vault key, is ever written to
//! `localStorage`, `sessionStorage`, IndexedDB, or disk (see
//! [`crate::session`]); the vault key is re-derived from the credential each
//! time the tab loads and lives only in process memory.
//!
//! # Locked sessions list, sealed
//!
//! The vault key comes from [`crate::crypto::SessionKeys`], which exists only
//! in an **unlocked** session ([`crate::session::Auth`]). After a reload the
//! screen is still reachable and the rows are still fetched and **listed** — as
//! sealed placeholders, with a working delete, exactly like an encrypted room's
//! sealed bubbles. [`sealed_labels`] is that state, and it is a state, not an
//! error: unlocking with the credential turns the seals into text.
//!
//! # Why decryption failure is a state, not an error
//!
//! [`Vault::open_label`] and [`Vault::open_value`] never return a `Result`. A
//! half that will not decrypt is a *thing on the screen* — the row has an id, a
//! timestamp, and a delete button that still works — and one such half must not
//! abort the batch it arrived in. The two fail independently for the same
//! reason: a corrupt value should not cost you the label that tells you which
//! account to go and reset.

use pocketskynet_core::secrets::{self, Field, SealedField, VaultKey, MAX_SECRET_BYTES};
use pocketskynet_core::CryptoError;

use crate::api::passwords::PasswordEntry;

/// One decrypted half of an entry.
///
/// `Sealed` carries no detail on purpose: "the MAC did not verify", "the
/// padding was wrong" and "that was not UTF-8" are the same fact to a reader
/// and three different hints to an attacker, and `core::secrets` refuses to
/// distinguish them for exactly that reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// Decrypted.
    Text(String),
    /// Present, authenticated against nothing this session can produce.
    Sealed,
}

/// An entry as the **list** sees it: its label opened where possible, and its
/// id and timestamps — but **not** its value.
///
/// The absence of a value field is the whole point (see the module docs): the
/// list is built from these and memoised, so caching one keeps only the label
/// resident, never a password. The value is fetched on demand from the sealed
/// [`PasswordEntry`] the moment it is revealed or copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretLabel {
    pub id: String,
    pub key: Opened,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SecretLabel {
    /// Whether the label decrypted.
    ///
    /// The list gates Reveal, Copy and Edit on this: a row whose *label* this
    /// session cannot read is one sealed to another wallet entirely, and the
    /// only sensible action left is delete. (A row whose label reads but whose
    /// value is corrupt is discovered at reveal time, and shows sealed there.)
    pub fn is_readable(&self) -> bool {
        matches!(self.key, Opened::Text(_))
    }

    /// Whether this entry matches a filter string.
    ///
    /// Case-insensitive, over the **label only** — there is no value here to
    /// search even if we wanted to, which is the design: finding "the entry
    /// whose password is hunter2" is a capability a shoulder-surfer benefits
    /// from more than its owner does.
    ///
    /// An empty needle matches everything, including sealed entries — a filter
    /// nobody typed must not hide rows.
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        match &self.key {
            Opened::Text(k) => k.to_lowercase().contains(&needle),
            // A sealed label cannot be searched, and claiming it matched would
            // put a row the user cannot read at the top of their results.
            Opened::Sealed => false,
        }
    }
}

/// Why a seal could not be produced.
///
/// Distinguished so the UI can say the right thing: a value past the cap is the
/// user's to shorten, a dead CSPRNG is the browser's fault and nothing was
/// stored either way. Collapsing them into one "this device failed" message —
/// which an earlier version did — told a user who pasted a 5 KB blob to go buy
/// a new browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// A field exceeded [`pocketskynet_core::secrets::MAX_SECRET_BYTES`].
    TooLong,
    /// Sealing itself failed — in practice a CSPRNG refusal.
    Failed,
}

/// The session's password vault.
///
/// Holds only the derived [`VaultKey`] (itself two subkeys, never the wallet
/// key). Built fresh from the session for each transient use — a list-label
/// pass, one reveal, one seal — and dropped when that use ends; nothing here is
/// stored across a sign-out.
pub struct Vault {
    key: VaultKey,
}

impl Vault {
    /// Build from an already-derived key.
    ///
    /// The session hands out a [`VaultKey`] (`SessionKeys::vault_key`) and the
    /// component clones it once — into the label memo, and into each transient
    /// reveal/seal — rather than re-borrowing the session on every use. There is
    /// deliberately no `for_session` convenience: it would encourage building a
    /// `Vault` that outlives the one action it was made for, which is exactly
    /// the resident-key habit this module avoids.
    pub fn from_key(key: VaultKey) -> Self {
        Self { key }
    }

    /// Open **only the label** of one entry. Cheap, and safe to keep — a label
    /// is not a secret value.
    pub fn open_label(&self, entry: &PasswordEntry) -> SecretLabel {
        let key = match secrets::open_field(&self.key, &entry.id, Field::Key, &entry.key) {
            Ok(text) => Opened::Text(text),
            Err(_) => Opened::Sealed,
        };
        SecretLabel {
            id: entry.id.clone(),
            key,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }

    /// Open **only the value** of one entry, on demand.
    ///
    /// The one door to a decrypted password. Callers must use the result within
    /// the action that asked for it (a reveal, a copy, an edit prefill) and let
    /// it drop — never store it. There is deliberately no batch form.
    pub fn open_value(&self, entry: &PasswordEntry) -> Opened {
        match secrets::open_field(&self.key, &entry.id, Field::Value, &entry.value) {
            Ok(text) => Opened::Text(text),
            Err(_) => Opened::Sealed,
        }
    }

    /// Open every entry's **label** for the list, in the order it arrived.
    ///
    /// The server orders by `updatedAt` and this preserves it. Re-sorting on
    /// the decrypted label would look tidier and would bury a just-saved entry
    /// in the middle of a long list — the one moment a person needs to see it.
    pub fn labels(&self, entries: &[PasswordEntry]) -> Vec<SecretLabel> {
        entries.iter().map(|e| self.open_label(e)).collect()
    }

    /// Seal a key/value pair for a given entry id.
    ///
    /// The id must be the one the entry will be stored under — it is inside
    /// both MACs. The oversize case is reported distinctly from a CSPRNG
    /// failure so the UI can tell the user which one it was.
    pub fn seal(
        &self,
        entry_id: &str,
        key_text: &str,
        value_text: &str,
    ) -> Result<(SealedField, SealedField), SealError> {
        // Check the cap first, so "your secret is too long" is reported as
        // itself rather than as the opaque `InvalidEncoding` core would also
        // return for a malformed id (which cannot happen here — the id is
        // minted or validated) folded into a device-failure message.
        if key_text.len() > MAX_SECRET_BYTES || value_text.len() > MAX_SECRET_BYTES {
            return Err(SealError::TooLong);
        }
        let key = secrets::seal_field(&self.key, entry_id, Field::Key, key_text)
            .map_err(map_seal_error)?;
        let value = secrets::seal_field(&self.key, entry_id, Field::Value, value_text)
            .map_err(map_seal_error)?;
        Ok((key, value))
    }
}

/// Map a core sealing error, having already ruled out the oversize case.
fn map_seal_error(err: CryptoError) -> SealError {
    match err {
        // The size cap is handled in `Vault::seal` before this runs, so an
        // `InvalidEncoding` reaching here would be a malformed id — which the
        // caller does not produce. Either way there is nothing the user can do
        // but retry, so it reads as a generic failure.
        CryptoError::Randomness | CryptoError::InvalidEncoding => SealError::Failed,
        _ => SealError::Failed,
    }
}

/// The sealed labels a **locked** session lists.
///
/// Every row present, every label sealed, ids and timestamps intact — so the
/// screen shows what is there and offers delete, and unlocking replaces the
/// seals with text. This is what the "locked lists, sealed" promise in the
/// module docs, `route.rs`, and the component all resolve to.
pub fn sealed_labels(entries: &[PasswordEntry]) -> Vec<SecretLabel> {
    entries
        .iter()
        .map(|e| SecretLabel {
            id: e.id.clone(),
            key: Opened::Sealed,
            created_at: e.created_at,
            updated_at: e.updated_at,
        })
        .collect()
}

/// Mint an id for a new entry.
///
/// Re-exported here so a component never has to reach into
/// `pocketskynet_core::secrets` directly — the seal and the id have to agree
/// about the charset, and keeping both behind this module is what makes that
/// impossible to get wrong.
pub fn new_entry_id() -> Result<String, CryptoError> {
    secrets::new_entry_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vault built straight from key material, bypassing `SessionKeys` —
    /// which needs a wallet and a salt, neither of which this module's logic
    /// depends on.
    fn vault(seed: u8) -> Vault {
        Vault {
            key: VaultKey::derive(&[seed; 32]),
        }
    }

    fn stored(vault: &Vault, id: &str, key: &str, value: &str) -> PasswordEntry {
        let (k, v) = vault.seal(id, key, value).unwrap();
        PasswordEntry {
            id: id.to_owned(),
            key: k,
            value: v,
            enc_ver: pocketskynet_core::secrets::SECRET_ENC_VER,
            created_at: 1_000,
            updated_at: 2_000,
        }
    }

    const ID: &str = "sec_00112233445566778899aabbccddeeff";

    #[test]
    fn a_label_opens_but_the_value_is_a_separate_on_demand_call() {
        // The core of the no-resident-plaintext design: listing an entry never
        // touches the value. `open_label` returns a struct with no value field
        // at all, and the value comes back only from an explicit `open_value`.
        let v = vault(1);
        let entry = stored(&v, ID, "chase.com", "correct horse battery staple");

        let label = v.open_label(&entry);
        assert_eq!(label.key, Opened::Text("chase.com".into()));
        assert!(label.is_readable());
        assert_eq!(label.id, ID);
        assert_eq!((label.created_at, label.updated_at), (1_000, 2_000));

        // The value is not on the label; it is fetched separately, on demand.
        assert_eq!(
            v.open_value(&entry),
            Opened::Text("correct horse battery staple".into())
        );
    }

    #[test]
    fn another_wallets_vault_key_sees_a_sealed_label_rather_than_an_error() {
        let mine = vault(1);
        let theirs = vault(2);
        let entry = stored(&mine, ID, "chase.com", "hunter2");

        let label = theirs.open_label(&entry);
        assert_eq!(label.key, Opened::Sealed);
        assert!(!label.is_readable());
        assert_eq!(label.id, ID, "the row is still identifiable");
        assert_eq!(theirs.open_value(&entry), Opened::Sealed);
    }

    #[test]
    fn a_locked_session_lists_every_row_sealed() {
        // The locked state is a *listing*, not a wall: ids and timestamps are
        // present so the rows render and can be deleted; only the labels are
        // sealed, and unlocking replaces them.
        let v = vault(1);
        let rows = vec![
            stored(&v, "sec_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "chase.com", "a"),
            stored(&v, "sec_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "gmail.com", "b"),
        ];
        let sealed = sealed_labels(&rows);
        assert_eq!(sealed.len(), 2);
        for label in &sealed {
            assert_eq!(label.key, Opened::Sealed);
            assert!(!label.is_readable());
            assert!(!label.id.is_empty(), "the id survives for delete");
        }
        // Order and timestamps are preserved, so the list is stable across a
        // lock/unlock.
        assert_eq!(sealed[0].id, "sec_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(sealed[0].updated_at, 2_000);
    }

    #[test]
    fn one_corrupt_value_leaves_the_label_readable() {
        // A damaged value must leave the label readable — that label is what
        // tells you which account to go and reset. The list shows the label;
        // the reveal shows sealed.
        let v = vault(1);
        let mut entry = stored(&v, ID, "chase.com", "hunter2");
        entry.value.ciphertext = "AAAA".to_owned();

        assert_eq!(v.open_label(&entry).key, Opened::Text("chase.com".into()));
        assert_eq!(v.open_value(&entry), Opened::Sealed);
    }

    #[test]
    fn a_row_whose_id_was_swapped_underneath_it_will_not_open() {
        let v = vault(1);
        let mut entry = stored(&v, ID, "chase.com", "hunter2");
        entry.id = "sec_ffeeddccbbaa99887766554433221100".to_owned();
        assert_eq!(v.open_label(&entry).key, Opened::Sealed);
        assert_eq!(v.open_value(&entry), Opened::Sealed);
    }

    #[test]
    fn the_two_halves_cannot_be_swapped_by_the_server() {
        let v = vault(1);
        let mut entry = stored(&v, ID, "chase.com", "hunter2");
        std::mem::swap(&mut entry.key, &mut entry.value);
        assert_eq!(
            v.open_label(&entry).key,
            Opened::Sealed,
            "a value must not read as a key"
        );
        assert_eq!(v.open_value(&entry), Opened::Sealed);
    }

    #[test]
    fn a_value_from_another_entry_will_not_open_in_this_one() {
        let v = vault(1);
        let other = "sec_ffeeddccbbaa99887766554433221100";
        let mut mine = stored(&v, ID, "chase.com", "hunter2");
        let theirs = stored(&v, other, "gmail.com", "swordfish");
        mine.value = theirs.value;
        assert_eq!(v.open_value(&mine), Opened::Sealed);
    }

    #[test]
    fn sealing_the_same_pair_twice_produces_different_ciphertext() {
        let v = vault(1);
        let (k1, v1) = v.seal(ID, "chase.com", "hunter2").unwrap();
        let (k2, v2) = v.seal(ID, "chase.com", "hunter2").unwrap();
        assert_ne!(k1.ciphertext, k2.ciphertext);
        assert_ne!(v1.ciphertext, v2.ciphertext);
        assert_ne!(v1.iv, v2.iv);
    }

    #[test]
    fn an_edit_leaves_nothing_of_the_previous_value_in_the_new_ciphertext() {
        let v = vault(1);
        let before = stored(&v, ID, "chase.com", "hunter2");
        let (k, val) = v.seal(ID, "chase.com", "hunter3").unwrap();
        assert_ne!(val.ciphertext, before.value.ciphertext);
        assert_ne!(val.iv, before.value.iv);
        assert_ne!(k.ciphertext, before.key.ciphertext, "even an unchanged key");
        assert_eq!(v.open_value(&before), Opened::Text("hunter2".into()));
    }

    #[test]
    fn an_oversize_secret_is_a_distinct_error_from_a_device_failure() {
        // The fix for the mislabelled refusal: a value past the cap must report
        // TooLong, not the generic Failed a CSPRNG death would give.
        let v = vault(1);
        let huge = "x".repeat(MAX_SECRET_BYTES + 1);
        assert_eq!(v.seal(ID, "name", &huge), Err(SealError::TooLong));
        assert_eq!(v.seal(ID, &huge, "value"), Err(SealError::TooLong));
        // Exactly at the cap is fine.
        assert!(v.seal(ID, "name", &"x".repeat(MAX_SECRET_BYTES)).is_ok());
    }

    #[test]
    fn a_minted_id_is_one_the_sealer_accepts() {
        let v = vault(1);
        let id = new_entry_id().unwrap();
        assert!(v.seal(&id, "k", "v").is_ok());
    }

    #[test]
    fn filtering_reads_the_label_and_has_no_value_to_read() {
        let v = vault(1);
        let label = v.open_label(&stored(&v, ID, "Chase Bank", "swordfish"));
        assert!(label.matches("chase"), "case-insensitive on the label");
        assert!(label.matches("BANK"));
        assert!(label.matches(""), "an empty filter hides nothing");
        assert!(label.matches("  "), "nor does whitespace");
        assert!(
            !label.matches("swordfish"),
            "the value is not even present here"
        );
        assert!(!label.matches("gmail"));
    }

    #[test]
    fn a_sealed_label_is_listed_but_never_claims_to_match() {
        let mine = vault(1);
        let theirs = vault(2);
        let label = theirs.open_label(&stored(&mine, ID, "chase.com", "hunter2"));
        assert!(label.matches(""), "still listed when nothing is filtered");
        assert!(!label.matches("chase"), "and never claims a match");
    }

    #[test]
    fn labels_preserve_the_servers_order() {
        let v = vault(1);
        let rows = vec![
            stored(&v, "sec_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "zulu", "1"),
            stored(&v, "sec_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "alpha", "2"),
        ];
        let labels = v.labels(&rows);
        let names: Vec<&str> = labels
            .iter()
            .map(|e| match &e.key {
                Opened::Text(s) => s.as_str(),
                Opened::Sealed => "",
            })
            .collect();
        assert_eq!(names, vec!["zulu", "alpha"]);
    }

    #[test]
    fn an_empty_value_round_trips() {
        let v = vault(1);
        let entry = stored(&v, ID, "the router", "");
        assert_eq!(v.open_label(&entry).key, Opened::Text("the router".into()));
        assert_eq!(v.open_value(&entry), Opened::Text(String::new()));
    }
}
