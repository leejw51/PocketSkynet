//! Challenge/response login and profile management (`docs/API.md` §6.2).

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::{
    challenge_message, random_hex_32, verify_challenge_signature, verify_key_binding, AuthUser,
};
use crate::db::{models::iso_ms, now_ms, users};
use crate::error::{ApiError, ApiResult};
use crate::validate::{self, ValidJson};
use crate::AppState;

/// How long a challenge stays valid. Long enough for a hardware wallet
/// confirmation, short enough that a leaked challenge is uninteresting.
const CHALLENGE_TTL_MS: i64 = 10 * 60 * 1000;

pub fn router(state: &AppState) -> Router<AppState> {
    // The two auth limiters are per-route and cumulative with the general
    // 100/min budget, so brute-forcing a signature costs the attacker their
    // whole API allowance as well.
    let challenge_route = Router::new()
        .route("/auth/challenge", post(challenge))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::ratelimit::challenge,
        ));

    let login_route = Router::new().route("/auth/login", post(login)).route_layer(
        axum::middleware::from_fn_with_state(state.clone(), crate::ratelimit::login),
    );

    Router::new()
        .merge(challenge_route)
        .merge(login_route)
        .route("/auth/logout", post(logout))
        .route("/auth/encryption-salt", get(encryption_salt))
        .route("/auth/encryption-key", put(encryption_key))
        .route("/auth/profile", get(profile).put(update_profile))
}

#[derive(Debug, Deserialize)]
struct ChallengeBody {
    #[serde(rename = "walletAddress")]
    wallet_address: Option<String>,
}

/// `POST /api/auth/challenge` — hand out something to sign.
async fn challenge(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<ChallengeBody>,
) -> ApiResult<Response> {
    let wallet = validate::wallet_address("walletAddress", body.wallet_address.as_deref())?;

    let nonce = random_hex_32()?;
    let message = challenge_message(&wallet, &nonce);
    let id = uuid::Uuid::new_v4().to_string();
    let expires_at = now_ms() + CHALLENGE_TTL_MS;

    let address = wallet.as_str().to_owned();
    let stored_message = message.clone();
    let stored_id = id.clone();
    state
        .db
        .call(move |conn| {
            // Opportunistic GC keeps the table bounded without a background
            // task; every login attempt pays a fraction of a millisecond.
            users::gc_challenges(conn)?;
            users::insert_challenge(
                conn,
                &stored_id,
                &address,
                &nonce,
                &stored_message,
                expires_at,
            )
        })
        .await?;

    Ok(Json(serde_json::json!({
        "challengeId": id,
        "message": message,
        "expiresAt": iso_ms(expires_at),
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct LoginBody {
    #[serde(rename = "walletAddress")]
    wallet_address: Option<String>,
    /// Accepts any JSON type, because the reference did and clients send
    /// `null` on repeat logins.
    username: Option<serde_json::Value>,
    #[serde(rename = "challengeId")]
    challenge_id: Option<String>,
    signature: Option<String>,
    #[serde(rename = "publicKey")]
    public_key: Option<String>,
    #[serde(rename = "publicKeySig")]
    public_key_sig: Option<String>,
}

/// JavaScript's `String(v || "")`: the falsy values all collapse to empty.
fn js_string_or_empty(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::Bool(false)) => String::new(),
        Some(serde_json::Value::Bool(true)) => "true".to_owned(),
        Some(serde_json::Value::String(s)) => s.trim().to_owned(),
        Some(serde_json::Value::Number(n)) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Some(other) => other.to_string(),
    }
}

/// `POST /api/auth/login` — verify a signature and mint a token.
///
/// The challenge is consumed **before** anything is validated, so a failed
/// attempt burns it. That is what makes it single-use: a replayed signature
/// has nothing left to verify against, and the 5/min limiter caps how fast an
/// attacker can obtain fresh material.
async fn login(
    State(state): State<AppState>,
    ValidJson(body): ValidJson<LoginBody>,
) -> ApiResult<Response> {
    let wallet = validate::wallet_address("walletAddress", body.wallet_address.as_deref())?;
    let challenge_id = body
        .challenge_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| validate::required("challengeId", "Challenge ID"))?
        .to_owned();
    let signature = validate::signature("signature", body.signature.as_deref())?;
    let supplied_username = js_string_or_empty(body.username.as_ref());

    let challenge = state
        .db
        .call({
            let challenge_id = challenge_id.clone();
            move |conn| users::consume_challenge(conn, &challenge_id)
        })
        .await?
        .ok_or_else(|| ApiError::bad_request("Invalid or expired challenge"))?;

    if now_ms() > challenge.expires_at {
        return Err(ApiError::bad_request("Challenge has expired"));
    }
    if !challenge
        .wallet_address
        .eq_ignore_ascii_case(wallet.as_str())
    {
        return Err(ApiError::bad_request("Wallet address mismatch"));
    }

    verify_challenge_signature(&wallet, &challenge.message, &signature)?;

    // Username resolution: an explicit name wins, an existing one is reused,
    // and a first-time login without one is refused rather than inventing a
    // placeholder the user cannot recognise.
    let existing = state
        .db
        .call({
            let address = wallet.as_str().to_owned();
            move |conn| users::get_user(conn, &address)
        })
        .await?;

    let username = if supplied_username.is_empty() {
        existing
            .as_ref()
            .map(|u| u.username.clone())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| ApiError::bad_request("Username is required for first-time login"))?
    } else {
        validate::username(Some(&supplied_username))?
    };

    // Key binding is verified only when both halves are present. One half
    // alone proves nothing, and rejecting it would break clients that publish
    // the key separately through PUT /api/auth/encryption-key.
    let (public_key, public_key_sig) = match (&body.public_key, &body.public_key_sig) {
        (Some(key), Some(sig)) => {
            let key = validate::public_key(Some(key))?;
            let sig = validate::signature("publicKeySig", Some(sig))?;
            if !verify_key_binding(&wallet, &key, &sig) {
                return Err(ApiError::bad_request(
                    "Invalid public key binding signature",
                ));
            }
            (Some(key), Some(sig))
        }
        // §15 #3: an absent key leaves *both* stored columns untouched.
        _ => (None, None),
    };

    // Refused here as well as at the extractor. The extractor is what makes
    // an *existing* token stop working; this is what stops a suspended wallet
    // from simply signing in again and getting a fresh one.
    if state.is_suspended(wallet.as_str()) {
        return Err(ApiError::forbidden(
            "This account has been suspended by a server administrator.",
        ));
    }

    let address = wallet.as_str().to_owned();
    let (user, salt) = state
        .db
        .call(move |conn| {
            let user = users::upsert_user(
                conn,
                &address,
                &username,
                public_key.as_deref(),
                public_key_sig.as_deref(),
            )?;
            let salt = users::get_or_create_salt(conn, &address)?;
            Ok((user, salt))
        })
        .await?;

    let token = state.jwt.issue(&wallet)?;

    // Logins are worth keeping outside the database: the users table records
    // that an account exists, not when it was accessed.
    let _ = state.log.append_audit(
        "login",
        Some(&wallet),
        serde_json::json!({ "challengeId": challenge_id }),
    );

    Ok(Json(serde_json::json!({
        "user": user,
        "token": token,
        "fruitnationWallet": super::misc::server_wallet(),
        "encryptionSalt": salt,
        // The client cannot work this out for itself: the admin list is
        // server-side configuration, and a client that guessed would either
        // hide a console its user is entitled to or offer one every request
        // behind it would refuse.
        "isServerAdmin": super::misc::is_server_admin(wallet.as_str()),
    }))
    .into_response())
}

/// `POST /api/auth/logout` — stateless.
///
/// There is no server-side session to end and no revocation list to add to;
/// the client discards its token. Returning 200 unconditionally, even without
/// a token, keeps client logout code branch-free.
async fn logout() -> Response {
    super::message("Logged out successfully")
}

/// `GET /api/auth/encryption-salt` — the caller's own derivation salt.
///
/// Never served for any other account. The salt plus the address reconstructs
/// the derivation message, and a signature over that message *is* the user's
/// E2EE private key — a public salt would let any page phish it.
async fn encryption_salt(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let salt = state
        .db
        .call(move |conn| users::get_or_create_salt(conn, &address))
        .await?;
    Ok(Json(serde_json::json!({ "salt": salt })).into_response())
}

#[derive(Debug, Deserialize)]
struct EncryptionKeyBody {
    #[serde(rename = "publicKey")]
    public_key: Option<String>,
    #[serde(rename = "publicKeySig")]
    public_key_sig: Option<String>,
}

/// `PUT /api/auth/encryption-key` — publish or rotate the caller's E2EE key.
async fn encryption_key(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<EncryptionKeyBody>,
) -> ApiResult<Response> {
    let public_key = validate::public_key(body.public_key.as_deref())?;
    let signature = validate::signature("publicKeySig", body.public_key_sig.as_deref())?;

    // The binding is re-derived from the *authenticated* address, never from
    // anything in the body: that is what stops one account publishing a key
    // bound to another.
    if !verify_key_binding(&caller, &public_key, &signature) {
        return Err(ApiError::bad_request(
            "Invalid public key binding signature",
        ));
    }

    let address = caller.as_str().to_owned();
    let stored_key = public_key.clone();
    let updated = state
        .db
        .call(move |conn| users::set_encryption_key(conn, &address, &stored_key, &signature))
        .await?;
    if !updated {
        return Err(ApiError::not_found("User not found"));
    }

    let _ = state.log.append_audit(
        "encryption_key_published",
        Some(&caller),
        serde_json::json!({ "publicKey": public_key }),
    );

    Ok(Json(serde_json::json!({
        "walletAddress": caller.as_str(),
        "publicKey": public_key,
    }))
    .into_response())
}

/// `GET /api/auth/profile`.
async fn profile(State(state): State<AppState>, AuthUser(caller): AuthUser) -> ApiResult<Response> {
    let address = caller.as_str().to_owned();
    let user = state
        .db
        .call(move |conn| users::get_user(conn, &address))
        .await?
        .ok_or_else(|| ApiError::not_found("User not found"))?;
    Ok(Json(user).into_response())
}

#[derive(Debug, Deserialize)]
struct ProfileBody {
    username: Option<String>,
    #[serde(rename = "profileImage")]
    profile_image: Option<String>,
}

/// `PUT /api/auth/profile` — username and chosen avatar.
///
/// `walletAddress` is the primary key and `publicKey`/`publicKeySig` move
/// together through their own endpoint, so neither is mutable here.
/// `profileImage` is optional and three-valued: absent leaves the stored
/// avatar alone, `""` clears it, and a value must be either `preset:<slug>`
/// or an `/api/images/…` URL this server hosts (see `validate::profile_image`).
async fn update_profile(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<ProfileBody>,
) -> ApiResult<Response> {
    let username = validate::username(body.username.as_deref())?;
    let profile_image = validate::profile_image(body.profile_image.as_deref())?;
    let address = caller.as_str().to_owned();

    let user = state
        .db
        .call(move |conn| {
            users::update_profile(
                conn,
                &address,
                &username,
                profile_image.as_ref().map(|i| i.as_deref()),
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found("User not found"))?;
    Ok(Json(user).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};
    use axum::http::StatusCode;

    #[tokio::test]
    async fn a_challenge_carries_the_exact_message_to_sign() {
        let router = build(state("challenge"));
        let address = wallet("alice").as_str().to_owned();

        let response = send(
            &router,
            "POST",
            "/api/auth/challenge",
            None,
            Some(serde_json::json!({ "walletAddress": address })),
        )
        .await;

        assert_eq!(response.status, StatusCode::OK);
        let message = response.json()["message"].as_str().unwrap();
        assert!(message.starts_with("Welcome to FruitNation!"));
        assert!(message.contains(&address));
        assert!(response.json()["challengeId"].is_string());
        assert!(response.json()["expiresAt"]
            .as_str()
            .unwrap()
            .ends_with('Z'));
    }

    #[tokio::test]
    async fn a_malformed_address_is_a_validation_error_not_a_crash() {
        let router = build(state("challenge-bad"));
        let response = send(
            &router,
            "POST",
            "/api/auth/challenge",
            None,
            Some(serde_json::json!({ "walletAddress": "nope" })),
        )
        .await;

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.json()["message"], "Validation failed");
        assert!(response.json()["errors"][0]
            .as_str()
            .unwrap()
            .starts_with("walletAddress:"));
    }

    #[tokio::test]
    async fn login_burns_the_challenge_even_when_it_fails() {
        let state = state("login-burn");
        let router = build(state.clone());
        let address = wallet("alice").as_str().to_owned();

        let challenge = send(
            &router,
            "POST",
            "/api/auth/challenge",
            None,
            Some(serde_json::json!({ "walletAddress": address })),
        )
        .await;
        let id = challenge.json()["challengeId"].as_str().unwrap().to_owned();

        let body = serde_json::json!({
            "walletAddress": address,
            "username": "alice",
            "challengeId": id,
            // Well-formed but not a signature over the challenge.
            "signature": format!("0x{}", "11".repeat(65)),
        });
        let first = send(&router, "POST", "/api/auth/login", None, Some(body.clone())).await;
        assert_eq!(first.status, StatusCode::UNAUTHORIZED);

        let second = send(&router, "POST", "/api/auth/login", None, Some(body)).await;
        assert_eq!(second.status, StatusCode::BAD_REQUEST);
        assert_eq!(second.json()["message"], "Invalid or expired challenge");
    }

    #[tokio::test]
    async fn login_refuses_a_challenge_issued_to_another_wallet() {
        let router = build(state("login-mismatch"));
        let alice = wallet("alice").as_str().to_owned();
        let bob = wallet("bob").as_str().to_owned();

        let challenge = send(
            &router,
            "POST",
            "/api/auth/challenge",
            None,
            Some(serde_json::json!({ "walletAddress": alice })),
        )
        .await;
        let id = challenge.json()["challengeId"].as_str().unwrap().to_owned();

        let response = send(
            &router,
            "POST",
            "/api/auth/login",
            None,
            Some(serde_json::json!({
                "walletAddress": bob,
                "username": "bob",
                "challengeId": id,
                "signature": format!("0x{}", "22".repeat(65)),
            })),
        )
        .await;

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.json()["message"], "Wallet address mismatch");
    }

    #[tokio::test]
    async fn an_unknown_challenge_id_is_refused() {
        let router = build(state("login-unknown"));
        let response = send(
            &router,
            "POST",
            "/api/auth/login",
            None,
            Some(serde_json::json!({
                "walletAddress": wallet("alice").as_str(),
                "username": "alice",
                "challengeId": "00000000-0000-0000-0000-000000000000",
                "signature": format!("0x{}", "33".repeat(65)),
            })),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.json()["message"], "Invalid or expired challenge");
    }

    #[test]
    fn javascript_falsy_usernames_collapse_to_empty() {
        use serde_json::json;
        assert_eq!(js_string_or_empty(None), "");
        assert_eq!(js_string_or_empty(Some(&json!(null))), "");
        assert_eq!(js_string_or_empty(Some(&json!(0))), "");
        assert_eq!(js_string_or_empty(Some(&json!(false))), "");
        assert_eq!(js_string_or_empty(Some(&json!("  alice  "))), "alice");
        assert_eq!(js_string_or_empty(Some(&json!(42))), "42");
    }

    #[tokio::test]
    async fn logout_succeeds_without_a_token() {
        let router = build(state("logout"));
        let response = send(&router, "POST", "/api/auth/logout", None, None).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json()["message"], "Logged out successfully");
    }

    #[tokio::test]
    async fn protected_endpoints_report_the_documented_401s() {
        let router = build(state("401"));

        let missing = send(&router, "GET", "/api/auth/profile", None, None).await;
        assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
        assert_eq!(missing.json()["message"], "No token provided");

        let bad = send(&router, "GET", "/api/auth/profile", Some("garbage"), None).await;
        assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
        assert_eq!(bad.json()["message"], "Invalid token");
    }

    #[tokio::test]
    async fn the_salt_is_stable_and_only_ever_the_callers_own() {
        let state = state("salt");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let first = send(
            &router,
            "GET",
            "/api/auth/encryption-salt",
            Some(&token),
            None,
        )
        .await;
        let second = send(
            &router,
            "GET",
            "/api/auth/encryption-salt",
            Some(&token),
            None,
        )
        .await;

        assert_eq!(first.status, StatusCode::OK);
        let salt = first.json()["salt"].as_str().unwrap();
        assert_eq!(salt.len(), 64);
        assert_eq!(salt, second.json()["salt"].as_str().unwrap());

        // There is deliberately no route that returns anybody else's salt.
        let other = send(
            &router,
            "GET",
            &format!("/api/users/{}/encryption-salt", wallet("bob").as_str()),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(other.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn publishing_a_key_with_a_bogus_binding_is_refused() {
        let state = state("keybinding");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let response = send(
            &router,
            "PUT",
            "/api/auth/encryption-key",
            Some(&token),
            Some(serde_json::json!({
                "publicKey": "04".to_owned() + &"ab".repeat(64),
                "publicKeySig": format!("0x{}", "44".repeat(65)),
            })),
        )
        .await;

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json()["message"],
            "Invalid public key binding signature"
        );
    }

    #[tokio::test]
    async fn the_profile_endpoints_read_and_rename() {
        let state = state("profile");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let got = send(&router, "GET", "/api/auth/profile", Some(&token), None).await;
        assert_eq!(got.json()["username"], "alice");
        assert_eq!(got.json()["walletAddress"], alice.as_str());
        assert!(got.json()["publicKey"].is_null());

        let renamed = send(
            &router,
            "PUT",
            "/api/auth/profile",
            Some(&token),
            Some(serde_json::json!({ "username": "  alice2  " })),
        )
        .await;
        assert_eq!(renamed.status, StatusCode::OK);
        assert_eq!(renamed.json()["username"], "alice2", "names are trimmed");
    }

    #[tokio::test]
    async fn a_profile_image_is_set_kept_across_renames_and_cleared() {
        let state = state("profile-image");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        // A gallery preset.
        let set = send(
            &router,
            "PUT",
            "/api/auth/profile",
            Some(&token),
            Some(serde_json::json!({ "username": "alice", "profileImage": "preset:tp-coder-f" })),
        )
        .await;
        assert_eq!(set.status, StatusCode::OK);
        assert_eq!(set.json()["profileImage"], "preset:tp-coder-f");

        // A rename that does not mention the avatar must not wipe it.
        let renamed = send(
            &router,
            "PUT",
            "/api/auth/profile",
            Some(&token),
            Some(serde_json::json!({ "username": "alice2" })),
        )
        .await;
        assert_eq!(renamed.json()["profileImage"], "preset:tp-coder-f");

        // A hosted-image URL, exactly the shape POST /api/images returns.
        let hosted = format!("/api/images/{}.png", "a".repeat(64));
        let uploaded = send(
            &router,
            "PUT",
            "/api/auth/profile",
            Some(&token),
            Some(serde_json::json!({ "username": "alice2", "profileImage": hosted })),
        )
        .await;
        assert_eq!(uploaded.json()["profileImage"], hosted);

        // The avatar is public: it rides along on GET /api/users/{address}.
        let seen = send(
            &router,
            "GET",
            &format!("/api/users/{}", alice.as_str()),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(seen.json()["profileImage"], hosted);

        // Empty string clears back to the hash-derived default.
        let cleared = send(
            &router,
            "PUT",
            "/api/auth/profile",
            Some(&token),
            Some(serde_json::json!({ "username": "alice2", "profileImage": "" })),
        )
        .await;
        assert!(cleared.json()["profileImage"].is_null());
    }

    #[tokio::test]
    async fn a_profile_image_outside_the_two_shapes_is_refused() {
        let state = state("profile-image-bad");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        for bad in [
            // Anything that could point another user's client off this server.
            "https://evil.example/pixel.png",
            "javascript:alert(1)",
            "//evil.example/x.png",
            // A traversal-shaped or malformed hosted name.
            "/api/images/../jwt.secret.png",
            "/api/images/notahash.png",
            &format!("/api/images/{}.exe", "a".repeat(64)),
            // A preset slug outside the slug alphabet.
            "preset:../../etc",
            "preset:UPPER",
            "preset:",
        ] {
            let response = send(
                &router,
                "PUT",
                "/api/auth/profile",
                Some(&token),
                Some(serde_json::json!({ "username": "alice", "profileImage": bad })),
            )
            .await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "accepted {bad:?}");
        }
    }

    #[tokio::test]
    async fn a_rename_to_markup_is_refused() {
        let state = state("profile-bad");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let response = send(
            &router,
            "PUT",
            "/api/auth/profile",
            Some(&token),
            Some(serde_json::json!({ "username": "<script>x</script>" })),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.json()["message"], "Validation failed");
    }
}
