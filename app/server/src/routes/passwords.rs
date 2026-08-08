//! Skynet Password — the encrypted key/value store (`docs/API.md` §18).
//!
//! Four verbs over one table, and the whole of the security model is two
//! sentences: every handler takes [`AuthUser`], so none of them can run
//! unauthenticated; and every database call takes the caller's address, so none
//! of them can reach another wallet's row. There is no admin override and no
//! shared entry — this is the one feature in the product with an audience of
//! exactly one.
//!
//! # Why a wrong owner gets 404 rather than 403
//!
//! Everywhere else in this server a caller who is authenticated but not
//! entitled gets a 403, because the resource is one they can reasonably know
//! exists — a room they were removed from, an admin route they are not an admin
//! for. An entry id is different: it is a 128-bit secret nobody but its owner
//! has any business holding, and a 403 would confirm the guess. So "somebody
//! else's entry" and "no such entry" are the same answer, and `db::passwords`
//! is written so they are literally the same code path rather than two
//! branches that have to be kept in step.
//!
//! # What the server sees
//!
//! Six base64/hex strings, an owner, and two timestamps. It cannot read a key,
//! a value, or tell two identical passwords apart — every seal draws a fresh IV.
//! It *can* see how many entries an account has and when each last changed;
//! `core/src/secrets.rs` says so plainly in its threat model rather than
//! leaving it to be discovered.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use pocketskynet_core::secrets::SealedField;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::db::passwords::{self, NewEntry};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

/// Most entries one account may hold.
///
/// A password store is a human-sized list. Five hundred is far past any real
/// one and still bounds what a compromised token can write into somebody
/// else's database before anyone notices — this is the only endpoint in the
/// product where an ordinary user creates unbounded rows that nothing else
/// cleans up.
const MAX_ENTRIES: i64 = 500;

/// The list ceiling. Equal to [`MAX_ENTRIES`], so a full account still comes
/// back in one response and the client never has to page an order it cannot
/// reproduce (it decrypts to filter — see the module docs of `db::passwords`).
const LIST_LIMIT: usize = MAX_ENTRIES as usize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/passwords", get(list).post(create))
        .route("/passwords/{id}", put(replace).delete(remove))
}

/// One sealed field as it arrives. Permissive by design — the validators below
/// are what produce field-named errors instead of serde's.
#[derive(Debug, Deserialize)]
struct SealedBody {
    ciphertext: Option<String>,
    iv: Option<String>,
    hmac: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EntryBody {
    /// Client-minted, and only read on create — a `PUT` takes its id from the
    /// path so a body that disagreed could never rename a row out from under
    /// its MAC.
    id: Option<String>,
    key: Option<SealedBody>,
    value: Option<SealedBody>,
    #[serde(rename = "encVer")]
    enc_ver: Option<i64>,
}

/// Validate one sealed field.
///
/// `field` names the half being checked (`key` or `value`) so the error message
/// says which one; the IV and MAC are fixed-width hex and are checked with the
/// same helper the room-key wraps use.
fn parse_sealed(field: &str, body: Option<&SealedBody>) -> ApiResult<SealedField> {
    let body = body.ok_or_else(|| validate::required(field, "A sealed field"))?;
    Ok(SealedField {
        ciphertext: validate::sealed_ciphertext(
            &format!("{field}.ciphertext"),
            body.ciphertext.as_deref(),
        )?,
        // Mixed-case hex is accepted on the way in, exactly as it is for room
        // keys: the MAC was computed over the string the client produced, and
        // normalising it here would be the server rewriting authenticated data.
        iv: validate::room_key_hex(&format!("{field}.iv"), body.iv.as_deref(), 32)?,
        hmac: validate::room_key_hex(&format!("{field}.hmac"), body.hmac.as_deref(), 64)?,
    })
}

/// `GET /api/passwords` — every entry this wallet holds, newest change first.
///
/// An empty store is a 200 with `[]`. "You have no secrets yet" is a normal
/// state, not a missing resource.
async fn list(State(state): State<AppState>, AuthUser(caller): AuthUser) -> ApiResult<Response> {
    let owner = caller.as_str().to_owned();
    let entries = state
        .db
        .call(move |conn| passwords::list(conn, &owner, LIST_LIMIT))
        .await?;
    Ok(Json(entries).into_response())
}

/// `POST /api/passwords` — store a new entry.
async fn create(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<EntryBody>,
) -> ApiResult<Response> {
    let id = validate::secret_entry_id(
        body.id
            .as_deref()
            .ok_or_else(|| validate::required("id", "An entry id"))?,
    )?;
    let new = NewEntry {
        id,
        owner_address: caller.as_str().to_owned(),
        key: parse_sealed("key", body.key.as_ref())?,
        value: parse_sealed("value", body.value.as_ref())?,
        enc_ver: validate::secret_enc_ver(body.enc_ver)?,
    };

    let entry = state
        .db
        .call(move |conn| {
            if passwords::count(conn, &new.owner_address)? >= MAX_ENTRIES {
                return Err(ApiError::bad_request(format!(
                    "You have reached the limit of {MAX_ENTRIES} saved passwords."
                )));
            }
            // `create` refuses a taken id inside its own statement, so a
            // retried create is a conflict rather than an overwrite — and two
            // concurrent ones cannot both win.
            passwords::create(conn, &new, now_ms())?
                .ok_or_else(|| ApiError::conflict("That entry already exists"))
        })
        .await?;

    Ok(Json(entry).into_response())
}

/// `PUT /api/passwords/{id}` — replace both halves of an entry.
///
/// The client re-seals with a fresh IV and sends the whole new ciphertext, so
/// nothing about the previous value crosses the wire — not a diff, not a
/// length, not a "same as before" flag. The old row is overwritten in place.
async fn replace(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
    ValidJson(body): ValidJson<EntryBody>,
) -> ApiResult<Response> {
    let id = validate::secret_entry_id(&id)?;
    let key = parse_sealed("key", body.key.as_ref())?;
    let value = parse_sealed("value", body.value.as_ref())?;
    let enc_ver = validate::secret_enc_ver(body.enc_ver)?;
    let owner = caller.as_str().to_owned();

    let entry = state
        .db
        .call(move |conn| passwords::replace(conn, &owner, &id, &key, &value, enc_ver, now_ms()))
        .await?
        .ok_or_else(not_found)?;

    Ok(Json(entry).into_response())
}

/// `DELETE /api/passwords/{id}`.
async fn remove(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id = validate::secret_entry_id(&id)?;
    let owner = caller.as_str().to_owned();

    let removed = state
        .db
        .call(move |conn| passwords::delete(conn, &owner, &id))
        .await?;
    if !removed {
        return Err(not_found());
    }
    Ok(super::message("Entry deleted"))
}

/// The one refusal this module makes for a missing *or* unowned entry.
///
/// A single constructor so the two can never drift into different wording —
/// which would reintroduce, in the message body, exactly the existence oracle
/// the shared status code exists to close.
fn not_found() -> ApiError {
    ApiError::not_found("Entry not found")
}

#[cfg(test)]
mod tests {
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;
    use serde_json::json;

    fn sealed(tag: &str) -> serde_json::Value {
        json!({
            "ciphertext": format!("Y2lwaGVydGV4dA=={tag}"),
            "iv": "0".repeat(32),
            "hmac": "a".repeat(64),
        })
    }

    fn entry(id: &str) -> serde_json::Value {
        json!({ "id": id, "key": sealed("k"), "value": sealed("v"), "encVer": 1 })
    }

    #[tokio::test]
    async fn every_route_refuses_an_unauthenticated_caller() {
        let router = build(state("passwords-anon"));
        let id = "sec_aaaaaaaaaaaaaaaa";

        for (method, path, body) in [
            ("GET", "/api/passwords".to_owned(), None),
            ("POST", "/api/passwords".to_owned(), Some(entry(id))),
            ("PUT", format!("/api/passwords/{id}"), Some(entry(id))),
            ("DELETE", format!("/api/passwords/{id}"), None),
        ] {
            let res = send(&router, method, &path, None, body).await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} must require a token"
            );
        }
    }

    #[tokio::test]
    async fn an_encryption_version_the_scheme_does_not_define_is_refused() {
        // `encVer: 2` is a valid *message* version and no password version at
        // all. It is not inside the sealing MAC, so if the bound accepted it a
        // client — or a tampering server — could plant rows no reader can open.
        let state = state("passwords-encver");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let mut bad = entry("sec_aaaaaaaaaaaaaaaa");
        bad["encVer"] = json!(2);
        let created = send(&router, "POST", "/api/passwords", Some(&token), Some(bad)).await;
        assert_eq!(created.status, StatusCode::BAD_REQUEST);

        // The real version still works, and a missing one defaults to it.
        let ok = send(
            &router,
            "POST",
            "/api/passwords",
            Some(&token),
            Some(entry("sec_bbbbbbbbbbbbbbbb")),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);
        assert_eq!(ok.json()["encVer"], 1);

        let mut no_ver = entry("sec_cccccccccccccccc");
        no_ver.as_object_mut().unwrap().remove("encVer");
        let defaulted = send(
            &router,
            "POST",
            "/api/passwords",
            Some(&token),
            Some(no_ver),
        )
        .await;
        assert_eq!(defaulted.status, StatusCode::OK);
        assert_eq!(defaulted.json()["encVer"], 1);

        // And an edit cannot smuggle a bad version in either.
        let bump = send(
            &router,
            "PUT",
            "/api/passwords/sec_bbbbbbbbbbbbbbbb",
            Some(&token),
            Some(json!({ "key": sealed("k2"), "value": sealed("v2"), "encVer": 2 })),
        )
        .await;
        assert_eq!(bump.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_malformed_sealed_field_is_a_named_validation_error() {
        let state = state("passwords-validate");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        // A 31-character IV: the kind of truncation a buggy client produces.
        let mut bad = entry("sec_aaaaaaaaaaaaaaaa");
        bad["value"]["iv"] = json!("0".repeat(31));
        let res = send(&router, "POST", "/api/passwords", Some(&token), Some(bad)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);
        let body = res.json().to_string();
        assert!(
            body.contains("value.iv"),
            "error must name the field: {body}"
        );

        // An id that could never have been minted.
        let mut bad = entry("sec_aaaaaaaaaaaaaaaa");
        bad["id"] = json!("has|a|pipe|in|it");
        let res = send(&router, "POST", "/api/passwords", Some(&token), Some(bad)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_second_wallet_gets_the_same_answer_as_a_stranger() {
        let state = state("passwords-scope");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        let id = "sec_aaaaaaaaaaaaaaaa";
        let made = send(
            &router,
            "POST",
            "/api/passwords",
            Some(&alice_token),
            Some(entry(id)),
        )
        .await;
        assert_eq!(made.status, StatusCode::OK, "alice can create");

        // Bob knows the id exactly. Every verb answers 404 — the same thing it
        // would say about an id that never existed.
        let known = send(
            &router,
            "PUT",
            &format!("/api/passwords/{id}"),
            Some(&bob_token),
            Some(entry(id)),
        )
        .await;
        let invented = send(
            &router,
            "PUT",
            "/api/passwords/sec_zzzzzzzzzzzzzzzz",
            Some(&bob_token),
            Some(entry("sec_zzzzzzzzzzzzzzzz")),
        )
        .await;
        assert_eq!(known.status, StatusCode::NOT_FOUND);
        assert_eq!(invented.status, StatusCode::NOT_FOUND);
        assert_eq!(
            known.json()["message"],
            invented.json()["message"],
            "the two must be indistinguishable in the body too"
        );

        assert_eq!(
            send(
                &router,
                "DELETE",
                &format!("/api/passwords/{id}"),
                Some(&bob_token),
                None
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
        assert!(
            send(&router, "GET", "/api/passwords", Some(&bob_token), None)
                .await
                .json()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
