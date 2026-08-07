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
//! # Locked sessions hold nothing
//!
//! The vault key comes from [`crate::crypto::SessionKeys`], which exists only
//! in an **unlocked** session ([`crate::session::Auth`]). After a reload the
//! screen is reachable and empty until the credential is supplied, exactly like
//! an encrypted room's sealed bubbles — and for the same reason, which is that
//! the alternative is persisting the key that opens all of it.
//!
//! # Why decryption failure is a state, not an error
//!
//! [`Vault::open`] never returns a `Result`. A row that will not decrypt is a
//! *thing on the screen* — it has an id, a timestamp, and a delete button that
//! still works — and one such row must not abort the batch it arrived in. The
//! two halves fail independently for the same reason: a corrupt value should
//! not cost you the label that tells you which account to go and reset.

use pocketskynet_core::secrets::{self, Field, SealedField, VaultKey, SECRET_ENC_VER};

use crate::api::passwords::PasswordEntry;
use crate::crypto::SessionKeys;

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

impl Opened {
    /// The text, or `None` for a sealed field.
    pub fn text(&self) -> Option<&str> {
        match self {
            Opened::Text(s) => Some(s),
            Opened::Sealed => None,
        }
    }

    /// The text, or the empty string. For the edit form, which cannot
    /// meaningfully pre-fill a field it could not read.
    pub fn text_or_empty(&self) -> &str {
        self.text().unwrap_or("")
    }
}

/// An entry as the screen sees it: opened where possible, still listed where
/// not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEntry {
    pub id: String,
    pub key: Opened,
    pub value: Opened,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SecretEntry {
    /// Whether both halves decrypted. An entry that is half readable is still
    /// shown, and is still editable — re-sealing both halves is how you repair
    /// one.
    pub fn is_readable(&self) -> bool {
        matches!(self.key, Opened::Text(_)) && matches!(self.value, Opened::Text(_))
    }

    /// Whether this entry matches a filter string.
    ///
    /// Case-insensitive, over the **key only**. Searching the values too would
    /// be a convenient way to find "the entry whose password is hunter2", which
    /// is a capability a shoulder-surfer benefits from more than you do.
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
            // A sealed key cannot be searched, and claiming it matched would
            // put a row the user cannot read at the top of their results.
            Opened::Sealed => false,
        }
    }
}

/// The session's password vault.
///
/// Not `Clone` and not `Debug`, for the reasons [`SessionKeys`] is neither: a
/// clone invites a copy that outlives a sign-out, and a derived `Debug` is how
/// key material reaches a console log.
pub struct Vault {
    key: VaultKey,
}

impl Vault {
    /// Derive the vault for an unlocked session.
    pub fn for_session(keys: &SessionKeys) -> Self {
        Self {
            key: keys.vault_key(),
        }
    }

    /// Seal a key/value pair for a given entry id.
    ///
    /// The id must be the one the entry will be stored under — it is inside
    /// both MACs. On an edit that is the existing id; on a create it is a fresh
    /// [`new_entry_id`].
    ///
    /// Fails only on a dead CSPRNG, a malformed id, or a field past
    /// `core::secrets::MAX_SECRET_BYTES`; the caller surfaces the refusal
    /// rather than storing something it could not seal.
    pub fn seal(
        &self,
        entry_id: &str,
        key_text: &str,
        value_text: &str,
    ) -> Result<(SealedField, SealedField), pocketskynet_core::CryptoError> {
        let key = secrets::seal_field(&self.key, entry_id, Field::Key, key_text)?;
        let value = secrets::seal_field(&self.key, entry_id, Field::Value, value_text)?;
        Ok((key, value))
    }

    /// Open one stored entry. Never fails; see the module docs.
    pub fn open(&self, entry: &PasswordEntry) -> SecretEntry {
        let open_one = |field: Field, sealed: &SealedField| match secrets::open_field(
            &self.key, &entry.id, field, sealed,
        ) {
            Ok(text) => Opened::Text(text),
            Err(_) => Opened::Sealed,
        };
        SecretEntry {
            key: open_one(Field::Key, &entry.key),
            value: open_one(Field::Value, &entry.value),
            id: entry.id.clone(),
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }

    /// Open a whole list, in the order it arrived.
    ///
    /// The server orders by `updatedAt` and this preserves that. Re-sorting
    /// here on the decrypted key would look tidier and would leak the sort
    /// order back to nobody — but it would also mean a newly saved entry
    /// appears somewhere in the middle of a long list, which is the one moment
    /// a person needs to see it.
    pub fn open_all(&self, entries: &[PasswordEntry]) -> Vec<SecretEntry> {
        entries.iter().map(|e| self.open(e)).collect()
    }
}

/// The `encVer` this client writes. One scheme so far.
pub const ENC_VER: i64 = SECRET_ENC_VER;

/// Mint an id for a new entry.
///
/// Re-exported here so a component never has to reach into
/// `pocketskynet_core::secrets` directly — the seal and the id have to agree
/// about the charset, and keeping both behind this module is what makes that
/// impossible to get wrong.
pub fn new_entry_id() -> Result<String, pocketskynet_core::CryptoError> {
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
            enc_ver: ENC_VER,
            created_at: 1_000,
            updated_at: 2_000,
        }
    }

    const ID: &str = "sec_00112233445566778899aabbccddeeff";

    #[test]
    fn an_entry_survives_the_round_trip_through_the_wire_shape() {
        let v = vault(1);
        let entry = stored(&v, ID, "chase.com", "correct horse battery staple");
        let opened = v.open(&entry);

        assert_eq!(opened.key, Opened::Text("chase.com".into()));
        assert_eq!(
            opened.value,
            Opened::Text("correct horse battery staple".into())
        );
        assert!(opened.is_readable());
        assert_eq!(opened.id, ID);
        assert_eq!((opened.created_at, opened.updated_at), (1_000, 2_000));
    }

    #[test]
    fn another_wallets_vault_key_sees_a_sealed_row_rather_than_an_error() {
        // The screen must still render: a row nobody can read has an id, a
        // date and a working delete button.
        let mine = vault(1);
        let theirs = vault(2);
        let entry = stored(&mine, ID, "chase.com", "hunter2");

        let opened = theirs.open(&entry);
        assert_eq!(opened.key, Opened::Sealed);
        assert_eq!(opened.value, Opened::Sealed);
        assert!(!opened.is_readable());
        assert_eq!(opened.id, ID, "the row is still identifiable");
        assert_eq!(opened.key.text(), None);
        assert_eq!(opened.key.text_or_empty(), "");
    }

    #[test]
    fn one_corrupt_half_does_not_cost_the_other() {
        // A damaged value must leave the label readable — that label is what
        // tells you which account to go and reset.
        let v = vault(1);
        let mut entry = stored(&v, ID, "chase.com", "hunter2");
        entry.value.ciphertext = "AAAA".to_owned();

        let opened = v.open(&entry);
        assert_eq!(opened.key, Opened::Text("chase.com".into()));
        assert_eq!(opened.value, Opened::Sealed);
        assert!(!opened.is_readable());
    }

    #[test]
    fn a_row_whose_id_was_swapped_underneath_it_will_not_open() {
        // The server holds the id and the ciphertext separately. Renaming the
        // row must not silently relabel somebody's password.
        let v = vault(1);
        let mut entry = stored(&v, ID, "chase.com", "hunter2");
        entry.id = "sec_ffeeddccbbaa99887766554433221100".to_owned();

        let opened = v.open(&entry);
        assert_eq!(opened.key, Opened::Sealed);
        assert_eq!(opened.value, Opened::Sealed);
    }

    #[test]
    fn the_two_halves_cannot_be_swapped_by_the_server() {
        let v = vault(1);
        let mut entry = stored(&v, ID, "chase.com", "hunter2");
        std::mem::swap(&mut entry.key, &mut entry.value);

        let opened = v.open(&entry);
        assert_eq!(opened.key, Opened::Sealed, "a value must not read as a key");
        assert_eq!(opened.value, Opened::Sealed);
    }

    #[test]
    fn a_ciphertext_from_another_entry_will_not_open_in_this_one() {
        let v = vault(1);
        let other = "sec_ffeeddccbbaa99887766554433221100";
        let mine = stored(&v, ID, "chase.com", "hunter2");
        let theirs = stored(&v, other, "gmail.com", "swordfish");

        let mut frankenstein = mine;
        frankenstein.value = theirs.value;
        assert_eq!(v.open(&frankenstein).value, Opened::Sealed);
    }

    #[test]
    fn sealing_the_same_pair_twice_produces_different_ciphertext() {
        // Otherwise the server learns that an edit was a no-op, and which of
        // your entries share a password.
        let v = vault(1);
        let (k1, v1) = v.seal(ID, "chase.com", "hunter2").unwrap();
        let (k2, v2) = v.seal(ID, "chase.com", "hunter2").unwrap();
        assert_ne!(k1.ciphertext, k2.ciphertext);
        assert_ne!(v1.ciphertext, v2.ciphertext);
        assert_ne!(v1.iv, v2.iv);
    }

    #[test]
    fn an_edit_leaves_nothing_of_the_previous_value_in_the_new_ciphertext() {
        // The requirement in the brief, asserted where it is decided: an edit
        // is a fresh seal of the new plaintext, not a delta against the old.
        let v = vault(1);
        let before = stored(&v, ID, "chase.com", "hunter2");
        let (k, val) = v.seal(ID, "chase.com", "hunter3").unwrap();

        assert_ne!(val.ciphertext, before.value.ciphertext);
        assert_ne!(val.iv, before.value.iv);
        assert_ne!(k.ciphertext, before.key.ciphertext, "even an unchanged key");
        assert_eq!(v.open(&before).value, Opened::Text("hunter2".into()));
    }

    #[test]
    fn sealing_refuses_an_id_it_could_not_have_minted() {
        let v = vault(1);
        assert!(v.seal("bad|id", "k", "v").is_err());
        assert!(v.seal(ID, "k", "v").is_ok());
    }

    #[test]
    fn a_minted_id_is_one_the_sealer_accepts() {
        // The two halves of the contract this module keeps together.
        let v = vault(1);
        let id = new_entry_id().unwrap();
        assert!(v.seal(&id, "k", "v").is_ok());
    }

    #[test]
    fn filtering_reads_the_key_and_never_the_value() {
        let v = vault(1);
        let entry = v.open(&stored(&v, ID, "Chase Bank", "swordfish"));

        assert!(entry.matches("chase"), "case-insensitive on the key");
        assert!(entry.matches("BANK"));
        assert!(entry.matches(""), "an empty filter hides nothing");
        assert!(entry.matches("  "), "nor does whitespace");
        assert!(
            !entry.matches("swordfish"),
            "the value must not be searchable"
        );
        assert!(!entry.matches("gmail"));
    }

    #[test]
    fn a_sealed_entry_is_listed_but_never_claims_to_match() {
        let mine = vault(1);
        let theirs = vault(2);
        let entry = theirs.open(&stored(&mine, ID, "chase.com", "hunter2"));

        assert!(entry.matches(""), "still listed when nothing is filtered");
        assert!(!entry.matches("chase"), "and never claims a match");
    }

    #[test]
    fn open_all_preserves_the_servers_order() {
        // The server sorts by `updatedAt`; re-sorting on the decrypted key
        // would bury a just-saved entry in the middle of a long list.
        let v = vault(1);
        let rows = vec![
            stored(&v, "sec_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "zulu", "1"),
            stored(&v, "sec_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "alpha", "2"),
        ];
        let opened = v.open_all(&rows);
        assert_eq!(
            opened
                .iter()
                .map(|e| e.key.text_or_empty())
                .collect::<Vec<_>>(),
            vec!["zulu", "alpha"]
        );
    }

    #[test]
    fn an_empty_value_is_a_legitimate_entry() {
        // "I have not set this password yet" is a row somebody wants to keep.
        let v = vault(1);
        let entry = v.open(&stored(&v, ID, "the router", ""));
        assert_eq!(entry.value, Opened::Text(String::new()));
        assert!(entry.is_readable());
    }
}
