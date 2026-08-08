//! Skynet Password — the encrypted key/value store (API.md §18).
//!
//! Every value that crosses this module is already sealed. Nothing here takes a
//! plaintext, and that is enforced by the types: the request bodies carry
//! [`SealedField`]s, which only `crate::secrets` can build. A future call site
//! that wanted to "just send the label in the clear so the server can sort"
//! would have to change this file first, which is exactly the moment somebody
//! should have to read `core/src/secrets.rs` and find out why the key is
//! ciphertext too.
//!
//! The id is minted by the client and sent on create, unlike every other
//! resource in this API. It is part of the MAC input over each field, and the
//! ciphertext has to exist before the row does — see CRYPTO.md §14.2.

use gloo_net::http::Method;
use pocketskynet_core::secrets::SealedField;
use serde::{Deserialize, Serialize};

use super::{encode_segment, ApiResult, Client};

/// One stored entry, exactly as the server returns it.
///
/// Both halves are opaque here. Turning them into text is
/// [`crate::secrets::Vault::open`]'s job, and it needs the session keys this
/// module deliberately does not have.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordEntry {
    pub id: String,
    pub key: SealedField,
    pub value: SealedField,
    /// Defaulted so a row written by a build that predates the column still
    /// deserialises — the same treatment `encVer` gets everywhere else.
    #[serde(default = "default_enc_ver")]
    pub enc_ver: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_enc_ver() -> i64 {
    1
}

/// The create body. `id` is ours; the two fields are sealed.
///
/// `encVer` is stamped from the one core constant, not threaded in by callers:
/// every writer of this scheme agrees on the version by construction, and the
/// server rejects anything else (`validate::secret_enc_ver`). When a v2 is ever
/// defined it changes in `core::secrets` and every body here follows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody<'a> {
    id: &'a str,
    key: &'a SealedField,
    value: &'a SealedField,
    enc_ver: i64,
}

/// The replace body. No `id`: the path carries it, so a body that disagreed
/// could not rename a row out from under the MAC that covers its id.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplaceBody<'a> {
    key: &'a SealedField,
    value: &'a SealedField,
    enc_ver: i64,
}

impl Client {
    /// Every entry this wallet holds, most recently changed first.
    ///
    /// The rows come back sealed; opening them is [`crate::secrets`]'s job, and
    /// the client opens only the *label* of each for the list — a value is
    /// decrypted solely when it is revealed or copied.
    pub async fn passwords(&self) -> ApiResult<Vec<PasswordEntry>> {
        self.send(Method::GET, "/api/passwords").await
    }

    /// Store a new entry under an id this client minted.
    ///
    /// A 409 means the id is taken. Retrying with the *same* id will keep
    /// failing — mint a new one; the server refuses to treat this as an upsert
    /// precisely so a retried create cannot destroy an edit made in between.
    pub async fn create_password(
        &self,
        id: &str,
        key: &SealedField,
        value: &SealedField,
    ) -> ApiResult<PasswordEntry> {
        self.send_json(
            Method::POST,
            "/api/passwords",
            &CreateBody {
                id,
                key,
                value,
                enc_ver: pocketskynet_core::secrets::SECRET_ENC_VER,
            },
        )
        .await
    }

    /// Replace both halves of an entry.
    ///
    /// Both, always. The caller re-seals from plaintext it already holds, so
    /// sending only the changed field would save one ciphertext and cost the
    /// guarantee that the two halves share an `encVer`.
    pub async fn update_password(
        &self,
        id: &str,
        key: &SealedField,
        value: &SealedField,
    ) -> ApiResult<PasswordEntry> {
        self.send_json(
            Method::PUT,
            &format!("/api/passwords/{}", encode_segment(id)),
            &ReplaceBody {
                key,
                value,
                enc_ver: pocketskynet_core::secrets::SECRET_ENC_VER,
            },
        )
        .await
    }

    /// Delete one entry. 404 if this wallet does not have it — which is also
    /// what somebody else's entry looks like from here.
    pub async fn delete_password(&self, id: &str) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/passwords/{}", encode_segment(id)),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_deserialises_from_the_documented_shape() {
        let raw = serde_json::json!({
            "id": "sec_00112233445566778899aabbccddeeff",
            "key": { "ciphertext": "a2V5", "iv": "0".repeat(32), "hmac": "a".repeat(64) },
            "value": { "ciphertext": "dmFs", "iv": "1".repeat(32), "hmac": "b".repeat(64) },
            "encVer": 1,
            "createdAt": 1_754_630_400_000i64,
            "updatedAt": 1_754_630_400_001i64,
        });
        let entry: PasswordEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(entry.id, "sec_00112233445566778899aabbccddeeff");
        assert_eq!(entry.key.ciphertext, "a2V5");
        assert_eq!(entry.value.ciphertext, "dmFs");
        assert_eq!(entry.updated_at, 1_754_630_400_001);
    }

    #[test]
    fn a_row_without_enc_ver_reads_as_version_one() {
        // A server that predates the column must not make the whole list
        // undeserialisable — one missing field would seal every entry.
        let raw = serde_json::json!({
            "id": "sec_00112233445566778899aabbccddeeff",
            "key": { "ciphertext": "a2V5", "iv": "0".repeat(32), "hmac": "a".repeat(64) },
            "value": { "ciphertext": "dmFs", "iv": "1".repeat(32), "hmac": "b".repeat(64) },
            "createdAt": 1,
            "updatedAt": 1,
        });
        let entry: PasswordEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(entry.enc_ver, 1);
    }

    #[test]
    fn the_create_body_carries_the_id_and_the_replace_body_does_not() {
        // The asymmetry is load-bearing: the id is inside the MAC, so a `PUT`
        // must take it from the path where it cannot be contradicted.
        let field = SealedField {
            ciphertext: "a2V5".into(),
            iv: "0".repeat(32),
            hmac: "a".repeat(64),
        };
        let create = serde_json::to_value(CreateBody {
            id: "sec_00112233445566778899aabbccddeeff",
            key: &field,
            value: &field,
            enc_ver: 1,
        })
        .unwrap();
        let mut keys: Vec<&str> = create.as_object().unwrap().keys().map(|s| &**s).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["encVer", "id", "key", "value"]);

        let replace = serde_json::to_value(ReplaceBody {
            key: &field,
            value: &field,
            enc_ver: 1,
        })
        .unwrap();
        let mut keys: Vec<&str> = replace.as_object().unwrap().keys().map(|s| &**s).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["encVer", "key", "value"]);
    }

    #[test]
    fn nothing_in_a_request_body_can_carry_a_plaintext() {
        // The property this module exists to hold. Both bodies are built only
        // from `SealedField`s, so the serialised form has no member a
        // plaintext could occupy — asserted on the field names rather than on
        // a value, since a ciphertext is allowed to be any string.
        let field = SealedField {
            ciphertext: "a2V5".into(),
            iv: "0".repeat(32),
            hmac: "a".repeat(64),
        };
        let body = serde_json::to_value(ReplaceBody {
            key: &field,
            value: &field,
            enc_ver: 1,
        })
        .unwrap();
        for half in ["key", "value"] {
            let mut inner: Vec<&str> = body[half]
                .as_object()
                .unwrap()
                .keys()
                .map(|s| &**s)
                .collect();
            inner.sort_unstable();
            assert_eq!(inner, vec!["ciphertext", "hmac", "iv"], "{half}");
        }
    }
}
