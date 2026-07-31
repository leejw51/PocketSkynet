//! The device vault — the sign-in credential this browser was told to keep.
//!
//! # What this is for
//!
//! Without it, [`crate::session`]'s policy has one visible cost: the JWT
//! survives a reload but the keys do not, so every refresh lands in the
//! **locked** state and asks for a 12-word phrase again. People respond to that
//! by keeping the phrase somewhere convenient — a note, a chat to themselves, a
//! screenshot — which is strictly worse than this. So the credential can be
//! stored here, once, and the unlock becomes automatic.
//!
//! # Be clear about what it costs
//!
//! This is the **only** entry in `localStorage` that is key material, and it is
//! the whole account: anyone with read access to this origin's storage can sign
//! in as you and read every message you can. Storing it trades the XSS-blast-
//! radius argument in [`crate::session`] for convenience, and that is a real
//! trade, not a free one. Three things keep it honest:
//!
//! 1. **It is a preference, not a default of the code.** [`remember`] gates
//!    every write, the login screen shows the switch next to the credential
//!    field, and turning it off wipes what is already there.
//! 2. **Forgetting is one click**, from Settings, and it is durable — clearing
//!    the vault also clears the preference, so the next sign-in does not
//!    silently write it back.
//! 3. **Signing out clears it.** A signed-out device holds nothing; "sign out"
//!    that left the phrase behind would be a lie.
//!
//! # Why the credential, and not the derived keys
//!
//! Storing the mnemonic and re-deriving is *smaller* than storing the E2EE
//! private key and every unwrapped room key, and it keeps one rule: whatever is
//! in memory came from the credential. There is no second path by which a key
//! can appear, so there is no second path to audit.
//!
//! # Integrity, not confidentiality
//!
//! [`StoredWallet::load`] re-derives the wallet and checks it still produces the
//! address it was saved against, discarding the entry if not. That catches a
//! truncated write and a hand-edited storage entry alike — it cannot stop
//! someone who can *read* the entry, and nothing here pretends otherwise.
//! Encrypting it would need a key, and the only place to keep that key is the
//! same storage; that is theatre, so it is not done.

use pocketskynet_core::{Wallet, WalletAddress};
use serde::{Deserialize, Serialize};

use crate::session::backend;

const KEY_VAULT: &str = "ps-wallet";
const KEY_REMEMBER: &str = "ps-remember-wallet";

/// How a wallet was reached. Both are stored, because a session created with a
/// private key cannot be unlocked with a recovery phrase — remembering only the
/// phrase would leave key users exactly where they started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    /// A BIP-39 phrase and the index it was derived at. The index is part of
    /// the credential: the same phrase at index 1 is a different account.
    Mnemonic { phrase: String, index: u32 },
    /// A raw secp256k1 scalar, hex, as entered.
    PrivateKey { hex: String },
}

impl Credential {
    /// The phrase, when there is one. Settings shows it behind a reveal; a
    /// private key has nothing worth showing that the address does not.
    pub fn phrase(&self) -> Option<&str> {
        match self {
            Credential::Mnemonic { phrase, .. } => Some(phrase),
            Credential::PrivateKey { .. } => None,
        }
    }

    fn derive(&self) -> Option<Wallet> {
        match self {
            Credential::Mnemonic { phrase, index } => Wallet::from_mnemonic(phrase, *index).ok(),
            Credential::PrivateKey { hex } => Wallet::from_private_key_hex(hex).ok(),
        }
    }
}

/// A remembered account: who it is, and how to become it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWallet {
    pub username: String,
    pub wallet_address: WalletAddress,
    pub credential: Credential,
}

impl StoredWallet {
    /// Read the vault, or `None` if it is empty, unreadable, or does not
    /// re-derive the address it claims.
    ///
    /// A mismatch is *discarded*, not repaired: an entry that no longer derives
    /// its own address is either corrupt or was edited by something that is not
    /// this app, and signing in with either is worse than asking.
    pub fn load() -> Option<Self> {
        let stored: Self = backend::get(KEY_VAULT)?;
        match stored.credential.derive() {
            Some(w) if w.address() == &stored.wallet_address => Some(stored),
            _ => {
                clear();
                None
            }
        }
    }

    /// The vault, but only if it holds *this* account. The unlock screen uses
    /// this: a vault for a different wallet must not auto-sign-in over the
    /// session already on the device.
    pub fn load_for(address: &WalletAddress) -> Option<Self> {
        Self::load().filter(|s| &s.wallet_address == address)
    }

    /// Store this credential — unless the user has turned remembering off, in
    /// which case this is deliberately a no-op rather than an error. The one
    /// gate is here so no call site can forget it.
    pub fn save(&self) {
        if remember() {
            backend::set(KEY_VAULT, self);
        }
    }

    /// Re-derive the wallet. Infallible in practice — [`Self::load`] already
    /// proved it derives — but still fallible in type, because the alternative
    /// is an `unwrap` on stored data.
    pub fn wallet(&self) -> Option<Wallet> {
        self.credential.derive()
    }
}

/// Whether this device may remember a credential. Default **on**: the switch is
/// on screen next to the field it applies to, so it is a visible default rather
/// than a hidden one.
pub fn remember() -> bool {
    backend::get::<bool>(KEY_REMEMBER).unwrap_or(true)
}

/// Set the preference. Turning it off wipes what is already stored — a switch
/// that only governs *future* writes would leave the phrase sitting there after
/// the user just said not to keep it.
pub fn set_remember(on: bool) {
    backend::set(KEY_REMEMBER, &on);
    if !on {
        backend::delete(KEY_VAULT);
    }
}

/// Drop the stored credential, leaving the preference alone.
///
/// This is the **sign-out** case, including "sign in as someone else". The
/// device must not still hold the phrase of an account nobody is signed into —
/// but the user did not ask to stop remembering, and switching accounts is
/// usually a prelude to remembering a different one.
pub fn clear() {
    backend::delete(KEY_VAULT);
}

/// Forget the credential *and* the preference, so the next sign-in does not
/// write it straight back.
///
/// This is the **explicit** case — Settings → Forget. The distinction from
/// [`clear`] is the whole point: someone who has just said "stop keeping my
/// recovery phrase here" has not said it about this session only, and having it
/// reappear on the next sign-in would make the button a lie.
pub fn forget() {
    set_remember(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon \
                          abandon abandon about";
    const PHRASE_ADDR: &str = "0x9858effd232b4033e47d90003d41ec34ecaeda94";
    const KEY_ONE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const KEY_ONE_ADDR: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";

    fn addr(s: &str) -> WalletAddress {
        WalletAddress::new(s).unwrap()
    }

    fn stored(credential: Credential, address: &str) -> StoredWallet {
        StoredWallet {
            username: "AmberEnchanter2784".into(),
            wallet_address: addr(address),
            credential,
        }
    }

    #[test]
    fn a_remembered_phrase_derives_the_account_it_was_saved_against() {
        let v = stored(
            Credential::Mnemonic {
                phrase: PHRASE.into(),
                index: 0,
            },
            PHRASE_ADDR,
        );
        let w = v.wallet().expect("must re-derive");
        assert_eq!(w.address(), &v.wallet_address);
    }

    #[test]
    fn the_derivation_index_is_part_of_the_credential() {
        // The same phrase at another index is a different account. Storing the
        // phrase without the index would sign the user in as the wrong wallet
        // — silently, since both derivations succeed.
        let at_0 = Credential::Mnemonic {
            phrase: PHRASE.into(),
            index: 0,
        };
        let at_1 = Credential::Mnemonic {
            phrase: PHRASE.into(),
            index: 1,
        };
        assert_ne!(
            at_0.derive().unwrap().address(),
            at_1.derive().unwrap().address()
        );
    }

    #[test]
    fn a_private_key_is_remembered_too() {
        // A session created with a private key cannot be unlocked with a
        // recovery phrase; remembering only phrases would strand those users.
        let v = stored(
            Credential::PrivateKey {
                hex: KEY_ONE.into(),
            },
            KEY_ONE_ADDR,
        );
        assert_eq!(v.wallet().unwrap().address(), &addr(KEY_ONE_ADDR));
        assert!(v.credential.phrase().is_none());
    }

    #[test]
    fn a_credential_that_does_not_match_its_address_is_rejected() {
        // What `load` checks. Tested on the pieces because `load` reads
        // `localStorage`, which does not exist on the host.
        let tampered = stored(
            Credential::Mnemonic {
                phrase: PHRASE.into(),
                index: 0,
            },
            KEY_ONE_ADDR,
        );
        assert_ne!(
            tampered.wallet().unwrap().address(),
            &tampered.wallet_address,
            "the mismatch this guards against"
        );
    }

    #[test]
    fn a_malformed_credential_derives_nothing_rather_than_panicking() {
        assert!(Credential::Mnemonic {
            phrase: "not a real phrase".into(),
            index: 0
        }
        .derive()
        .is_none());
        assert!(Credential::PrivateKey { hex: "0xzz".into() }
            .derive()
            .is_none());
    }

    #[test]
    fn the_stored_shape_is_exactly_username_address_and_credential() {
        // The mirror of `session`'s no-key-material test: this one *is* allowed
        // to carry a secret, so what it asserts is that it carries nothing
        // *else* — no token, no salt, no room keys.
        let v = stored(
            Credential::Mnemonic {
                phrase: PHRASE.into(),
                index: 0,
            },
            PHRASE_ADDR,
        );
        let json = serde_json::to_value(&v).unwrap();
        let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(|s| &**s).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["credential", "username", "wallet_address"]);

        // Round-trips, including the tagged credential.
        let back: StoredWallet = serde_json::from_value(json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn the_credential_tag_survives_a_round_trip_for_both_variants() {
        for c in [
            Credential::Mnemonic {
                phrase: PHRASE.into(),
                index: 3,
            },
            Credential::PrivateKey {
                hex: KEY_ONE.into(),
            },
        ] {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<Credential>(&json).unwrap(), c);
        }
    }
}
