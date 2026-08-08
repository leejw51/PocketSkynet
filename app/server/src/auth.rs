//! Authentication: challenge messages, JWTs, and the `AuthUser` extractor.
//!
//! There is no session table and no per-token revocation list. A JWT is a
//! bearer credential that is valid until it expires, which is why the TTL is
//! configurable and why realtime connections re-check `exp` while they are
//! open rather than only at the handshake.
//!
//! What does exist is revocation by *account*: [`AuthUser`] refuses a token
//! whose wallet a server admin has suspended (`AppState::is_suspended`). That
//! is a deliberately coarser instrument than a `jti` deny list — it cannot end
//! one session and leave another — but it is the one an operator actually
//! reaches for, and it costs a set lookup rather than a table.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use pocketskynet_core::WalletAddress;
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::AppState;

/// The claim set. `walletAddress` is the only claim the server reads; `iat`
/// and `exp` exist for the token's own lifecycle.
///
/// No `sub`/`iss`/`aud`/`jti`: there is exactly one issuer and one audience,
/// and a `jti` would imply a revocation list this server does not keep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    #[serde(rename = "walletAddress")]
    pub wallet_address: String,
    pub iat: i64,
    pub exp: i64,
}

/// A capability to fetch **one** resource, carried in a query parameter
/// instead of a header.
///
/// This exists for exactly one reason: a browser downloading a 4 GB file has
/// to be the thing that writes it to disk, and a navigation cannot carry an
/// `Authorization` header. Fetching the bytes into the page first and handing
/// over a blob — what the client did while the cap was 25 MB — needs the whole
/// file in memory twice, which is not available at this size and is not a
/// budget that grows.
///
/// Three properties keep that from being a hole:
///
/// * **`scope` names a single resource.** A token minted for one attachment
///   opens that attachment and nothing else, so a leaked URL is not an
///   account.
/// * **Authorisation is still checked at request time.** The claim carries the
///   wallet it was issued to and the handler re-runs the same membership check
///   an `Authorization` header would have gone through, so leaving a room
///   invalidates every outstanding token for that room's files immediately.
/// * **It expires quickly** — see [`DOWNLOAD_TTL_SECONDS`].
///
/// Signed with a **different key** from [`Claims`] — see
/// [`JwtKeys::download_secret`]. That is not belt-and-braces, it is the load-
/// bearing part: `Claims` deserialises from JSON that carries extra members,
/// so a download token sharing the session secret would verify as a *session*
/// token, and this credential is one that ends up in browser history, in the
/// download manager, and in any log that records a URL. Domain-separating the
/// key makes that substitution impossible rather than merely unlikely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClaims {
    #[serde(rename = "walletAddress")]
    pub wallet_address: String,
    /// The one resource this token opens, as an opaque route-defined string.
    pub scope: String,
    pub iat: i64,
    pub exp: i64,
}

/// How long a download capability lives.
///
/// One hour, which is longer than it takes to *start* a download and long
/// enough to survive one. The window has to cover the whole transfer, not just
/// the first request: a browser that loses the network mid-file resumes with a
/// `Range` request against the same URL, and a token that died in the meantime
/// turns a resumable 4 GB download into a restart. Set against that, the token
/// opens one file, to one wallet, and only while that wallet can still see it.
pub const DOWNLOAD_TTL_SECONDS: i64 = 3600;

/// Signing and verification material.
pub struct JwtKeys {
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
    /// Signing and verification for [`DownloadClaims`], under a key derived
    /// from the session secret. A token minted with one key cannot verify
    /// under the other, so the two credentials cannot be swapped.
    download_encoding: EncodingKey,
    download_decoding: DecodingKey,
    /// Validation for [`DownloadClaims`]: additionally requires `scope`.
    download_validation: Validation,
    ttl_seconds: i64,
}

impl std::fmt::Debug for JwtKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtKeys")
            .field("ttl_seconds", &self.ttl_seconds)
            .finish_non_exhaustive()
    }
}

impl JwtKeys {
    pub fn new(secret: &[u8], ttl_hours: i64) -> Self {
        // Pinning the algorithm on *verification* is what blocks the classic
        // `alg: none` and HS/RS confusion substitutions. `jsonwebtoken`
        // defaults to trusting the header's `alg`, so this is not optional.
        let mut validation = Validation::new(Algorithm::HS256);
        validation.algorithms = vec![Algorithm::HS256];
        validation.validate_exp = true;
        // Nothing issues these claims but us, and requiring them would break
        // no token we mint — but spelling it out documents what is checked.
        validation.required_spec_claims = std::collections::HashSet::from(["exp".to_owned()]);

        // The same pinning, plus `scope`. Requiring a claim the session token
        // does not carry is what makes the two token kinds non-interchangeable
        // despite sharing a secret and an algorithm.
        let mut download_validation = validation.clone();
        download_validation.required_spec_claims =
            std::collections::HashSet::from(["exp".to_owned(), "scope".to_owned()]);

        let dl = Self::download_secret(secret);

        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
            validation,
            download_encoding: EncodingKey::from_secret(&dl),
            download_decoding: DecodingKey::from_secret(&dl),
            download_validation,
            ttl_seconds: ttl_hours.max(1) * 3600,
        }
    }

    /// The download-token key, derived from the session secret.
    ///
    /// One secret is configured and two are needed, so the second is derived
    /// rather than asked for: an operator cannot forget to set it, rotating
    /// the session secret rotates this too, and no deployment can accidentally
    /// run with the two keys equal. The label is what makes it a *different*
    /// key and not merely a hash of the same one — anything else hashing the
    /// secret for another purpose must use a different label.
    fn download_secret(secret: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"pocketskynet/download-token/v1\0");
        hasher.update(secret);
        hasher.finalize().into()
    }

    /// Mint a capability to download one resource.
    ///
    /// `scope` is the route's own name for the thing — an attachment id, an
    /// image's stored name — and is compared verbatim on the way back in.
    pub fn issue_download(&self, wallet: &WalletAddress, scope: &str) -> ApiResult<String> {
        let now = now_secs();
        let claims = DownloadClaims {
            wallet_address: wallet.as_str().to_owned(),
            scope: scope.to_owned(),
            iat: now,
            exp: now + DOWNLOAD_TTL_SECONDS,
        };
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &self.download_encoding,
        )
        .map_err(|e| ApiError::Internal(anyhow::Error::new(e).context("signing download token")))
    }

    /// Verify a download capability **and** that it was minted for `scope`.
    ///
    /// The scope comparison is here rather than left to the caller on purpose:
    /// a handler that verified the signature and forgot to check what the
    /// token was *for* would accept any valid token for any file, which is the
    /// one mistake this whole mechanism exists to make impossible.
    pub fn verify_download(&self, token: &str, scope: &str) -> ApiResult<WalletAddress> {
        let data = jsonwebtoken::decode::<DownloadClaims>(
            token,
            &self.download_decoding,
            &self.download_validation,
        )
        .map_err(|_| ApiError::unauthorized("Invalid or expired download link"))?;
        if data.claims.scope != scope {
            return Err(ApiError::unauthorized("Invalid or expired download link"));
        }
        WalletAddress::new(&data.claims.wallet_address)
            .map_err(|_| ApiError::unauthorized("Invalid or expired download link"))
    }

    /// Mint a token for an already-authenticated wallet.
    pub fn issue(&self, wallet: &WalletAddress) -> ApiResult<String> {
        let now = now_secs();
        let claims = Claims {
            wallet_address: wallet.as_str().to_owned(),
            iat: now,
            exp: now + self.ttl_seconds,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|e| ApiError::Internal(anyhow::Error::new(e).context("signing token")))
    }

    /// Verify a token and normalise the wallet claim.
    ///
    /// The claim is re-parsed through [`WalletAddress`] rather than trusted:
    /// a token minted by an older build, or hand-crafted with a valid
    /// signature but a mixed-case address, would otherwise become a second
    /// identity that matches nothing in the database.
    pub fn verify(&self, token: &str) -> ApiResult<(WalletAddress, Claims)> {
        let data = jsonwebtoken::decode::<Claims>(token, &self.decoding, &self.validation)
            .map_err(|_| ApiError::unauthorized("Invalid token"))?;
        let wallet = WalletAddress::new(&data.claims.wallet_address)
            .map_err(|_| ApiError::unauthorized("Invalid token"))?;
        Ok((wallet, data.claims))
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Pull the credential out of an `Authorization` header.
///
/// §15 #16: the reference did `header.replace("Bearer ", "")` — a substring
/// replace, so `bearer <token>` failed, a bare token worked, and a token
/// containing the literal `Bearer ` was mangled. Here the scheme is matched
/// case-insensitively, and a bare token is still accepted because several
/// native clients send one.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    match raw.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => {
            let token = token.trim();
            (!token.is_empty()).then_some(token)
        }
        // Some other scheme (Basic, Digest…) carries nothing we can use.
        Some(_) => None,
        // A bare token — but `Bearer` on its own is a scheme with an empty
        // credential, not a credential that happens to spell "Bearer".
        None if raw.eq_ignore_ascii_case("bearer") => None,
        None => Some(raw),
    }
}

/// The authenticated caller. Extracting it is the authentication check: a
/// handler that takes `AuthUser` cannot accidentally run unauthenticated.
#[derive(Debug, Clone)]
pub struct AuthUser(pub WalletAddress);

impl AuthUser {
    pub fn address(&self) -> &str {
        self.0.as_str()
    }

    pub fn to_owned_address(&self) -> String {
        self.0.as_str().to_owned()
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(&parts.headers)
            .ok_or_else(|| ApiError::unauthorized("No token provided"))?;
        let (wallet, _) = state.jwt.verify(token)?;
        // The one place a suspension can take effect. A valid signature over a
        // valid token is still a valid token — there is no revocation list to
        // add it to — so "this account is no longer welcome" has to be decided
        // when the credential is presented, on every request, or not at all.
        if state.is_suspended(wallet.as_str()) {
            return Err(ApiError::unauthorized(
                "This account has been suspended by a server administrator.",
            ));
        }
        Ok(Self(wallet))
    }
}

/// An authenticated caller who administers this server.
///
/// Extracting it *is* the authorisation check, exactly as [`AuthUser`] is the
/// authentication one: a handler that takes this cannot be reached by anybody
/// else, and cannot forget to ask.
///
/// The role comes from `VITE_FRUITNATION_ADMIN` — see
/// [`crate::routes::misc::server_admins`] for why it is configuration rather
/// than a table.
#[derive(Debug, Clone)]
pub struct ServerAdmin(pub WalletAddress);

impl ServerAdmin {
    pub fn address(&self) -> &str {
        self.0.as_str()
    }
}

impl FromRequestParts<AppState> for ServerAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AuthUser(wallet) = AuthUser::from_request_parts(parts, state).await?;
        if !crate::routes::misc::is_server_admin(wallet.as_str()) {
            // 403, not 404: the caller is authenticated and this route exists.
            // Hiding it would only mean an operator who mistyped their own
            // address in `.env` sees "not found" and looks in the wrong place.
            return Err(ApiError::forbidden(
                "This action requires a server administrator.",
            ));
        }
        Ok(Self(wallet))
    }
}

/// The exact bytes a client signs to prove control of a wallet.
///
/// Reproduced verbatim from the reference, LF newlines, no trailing newline.
/// Any drift here breaks login for every existing client, since the signature
/// is over these bytes and nothing else.
pub fn challenge_message(wallet: &WalletAddress, nonce: &str) -> String {
    format!(
        "Welcome to FruitNation!\n\n\
         Click to sign in and accept the FruitNation Terms of Service.\n\n\
         This request will not trigger a blockchain transaction or cost any gas fees.\n\n\
         Wallet address:\n{}\n\n\
         Nonce:\n{}",
        wallet.as_str(),
        nonce
    )
}

/// The message binding an E2EE public key to a wallet.
///
/// Signed with the **wallet** key, never the encryption key — the whole point
/// is to prove that the holder of the wallet vouches for this encryption key.
/// Built in `pocketskynet_core` so the server and the browser client cannot
/// drift: a one-byte difference here rejects every key upload.
pub fn key_binding_message(wallet: &WalletAddress, public_key: &str) -> String {
    pocketskynet_core::keys::build_key_binding_message(wallet, public_key)
}

/// Verify that `signature` over the binding message recovers `wallet`.
///
/// Fails closed: a malformed signature, a key that is not a valid curve
/// point, or a recovered address that differs by so much as a byte all return
/// `false`. There is deliberately no warn-and-continue path — accepting an
/// unverified key destroys the end-to-end guarantee for a whole room, not for
/// one message.
///
/// The address fed in is always the **authenticated caller's**, never one
/// echoed from the request body; comparing a server-supplied address against a
/// server-supplied key would make the check vacuous.
pub fn verify_key_binding(wallet: &WalletAddress, public_key: &str, signature: &str) -> bool {
    pocketskynet_core::keys::verify_key_binding(wallet, Some(public_key), Some(signature)).is_ok()
}

/// Verify a login signature over a challenge message.
pub fn verify_challenge_signature(
    wallet: &WalletAddress,
    message: &str,
    signature: &str,
) -> ApiResult<()> {
    let recovered = pocketskynet_core::eip191::recover_address(message, signature)
        .map_err(|_| ApiError::unauthorized("Invalid signature format"))?;
    if &recovered == wallet {
        Ok(())
    } else {
        Err(ApiError::unauthorized("Invalid signature"))
    }
}

/// A **bearer token**: 32 bytes of CSPRNG output as lowercase hex.
///
/// Every unguessable *token* this server hands out is this function with a
/// prefix on the front, and there are exactly four: the login challenge nonce
/// (`routes/auth`), the SSE ticket (`routes/realtime`), the invite link
/// (`db::invites::mint_token`) and the webhook token (`routes/webhooks`). Each
/// is a bearer credential — whoever holds one is, for that purpose, whoever it
/// was issued to — so they share one generator, named for what they are.
///
/// The per-account encryption salt is *not* on this list even though it is the
/// same 64 hex characters: it is key-derivation input, not a credential
/// anybody presents, so it draws from [`pocketskynet_core::random::hex_32`]
/// directly rather than borrowing the bearer-token name. Keeping it off this
/// roster is what lets the roster be exact.
///
/// The bytes come from [`pocketskynet_core::random`] — see that module for why
/// the whole system has one entropy source and why a refusal is an error, not a
/// silent constant. Here that error is an [`ApiError::Internal`] via the `?`
/// below: the request becomes a 500 with the cause logged and nothing leaked,
/// and — the point — no token is minted.
pub fn random_hex_32() -> ApiResult<String> {
    Ok(pocketskynet_core::random::hex_32()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn keys() -> JwtKeys {
        JwtKeys::new(b"test-secret-that-is-at-least-32-bytes-long", 24)
    }

    fn wallet() -> WalletAddress {
        WalletAddress::new("0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22").unwrap()
    }

    fn header(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(value).unwrap(),
        );
        map
    }

    #[test]
    fn a_download_token_opens_the_scope_it_was_minted_for() {
        let keys = keys();
        let token = keys.issue_download(&wallet(), "file_1_abc").unwrap();
        assert_eq!(
            keys.verify_download(&token, "file_1_abc").unwrap(),
            wallet()
        );
    }

    #[test]
    fn a_download_token_does_not_open_a_different_file() {
        // The point of the scope claim. A valid signature is not authority
        // over everything the signer has ever signed for.
        let keys = keys();
        let token = keys.issue_download(&wallet(), "file_1_abc").unwrap();
        assert!(keys.verify_download(&token, "file_2_def").is_err());
        // Nor is a prefix or a suffix of the scope enough.
        assert!(keys.verify_download(&token, "file_1_ab").is_err());
        assert!(keys.verify_download(&token, "file_1_abcd").is_err());
    }

    #[test]
    fn the_two_token_kinds_are_not_interchangeable() {
        let keys = keys();

        // A session token is not a download capability. Caught by the key.
        let session = keys.issue(&wallet()).unwrap();
        assert!(keys.verify_download(&session, "file_1_abc").is_err());

        // And — the direction that matters — a download token is **not a
        // login**. `Claims` deserialises happily from a payload carrying an
        // extra `scope` member, so nothing in serde or in the validation stops
        // this; only the derived key does. A download URL is handed to the
        // browser and lands in history, in the download manager, and in every
        // log that records a URL, so if this assertion ever flips, one of
        // those is a credential.
        let download = keys.issue_download(&wallet(), "file_1_abc").unwrap();
        assert!(
            keys.verify(&download).is_err(),
            "a download link must never verify as a session token"
        );
    }

    #[test]
    fn the_download_key_is_not_the_session_key() {
        // The property the test above depends on, asserted directly so a
        // future refactor that collapses the two keys fails here first and
        // says why.
        let secret = b"test-secret-that-is-at-least-32-bytes-long";
        assert_ne!(&JwtKeys::download_secret(secret)[..], &secret[..]);
        // Deriving from a different session secret gives a different key, so
        // rotating the one rotates the other.
        assert_ne!(
            JwtKeys::download_secret(secret),
            JwtKeys::download_secret(b"a-completely-different-secret-value-32b")
        );
    }

    #[test]
    fn an_expired_download_token_is_refused() {
        let keys = keys();
        // Comfortably past `Validation`'s default 60-second leeway, which the
        // session tokens rely on too — an `exp` of exactly `now - 60` is still
        // inside it and would make this test assert nothing.
        let past = now_secs() - 3600;
        let claims = DownloadClaims {
            wallet_address: wallet().as_str().to_owned(),
            scope: "file_1_abc".to_owned(),
            iat: past - 60,
            exp: past,
        };
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(&JwtKeys::download_secret(
                b"test-secret-that-is-at-least-32-bytes-long",
            )),
        )
        .unwrap();
        assert!(keys.verify_download(&token, "file_1_abc").is_err());
    }

    #[test]
    fn a_download_token_signed_with_another_secret_is_refused() {
        let other = JwtKeys::new(b"a-completely-different-secret-value-32b", 24);
        let token = other.issue_download(&wallet(), "file_1_abc").unwrap();
        assert!(keys().verify_download(&token, "file_1_abc").is_err());
    }

    #[test]
    fn tokens_round_trip_and_carry_the_lowercase_address() {
        let keys = keys();
        let token = keys.issue(&wallet()).unwrap();
        let (recovered, claims) = keys.verify(&token).unwrap();

        assert_eq!(recovered, wallet());
        assert_eq!(claims.wallet_address, wallet().as_str());
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn a_token_signed_with_another_secret_is_rejected() {
        let token = keys().issue(&wallet()).unwrap();
        let other = JwtKeys::new(b"a-completely-different-secret-value-32b", 24);
        assert!(other.verify(&token).is_err());
    }

    #[test]
    fn alg_none_substitution_is_refused() {
        // Hand-rolled `{"alg":"none"}` header with the same claims. Without a
        // pinned algorithm list this verifies with an empty signature.
        let claims = serde_json::json!({
            "walletAddress": wallet().as_str(),
            "iat": now_secs(),
            "exp": now_secs() + 3600,
        });
        let b64 = |v: &[u8]| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v)
        };
        let forged = format!(
            "{}.{}.",
            b64(br#"{"alg":"none","typ":"JWT"}"#),
            b64(serde_json::to_string(&claims).unwrap().as_bytes())
        );
        assert!(keys().verify(&forged).is_err());
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let keys = JwtKeys::new(b"test-secret-that-is-at-least-32-bytes-long", 24);
        let claims = Claims {
            wallet_address: wallet().as_str().into(),
            iat: now_secs() - 7200,
            exp: now_secs() - 3600,
        };
        let expired = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret-that-is-at-least-32-bytes-long"),
        )
        .unwrap();
        assert!(keys.verify(&expired).is_err());
    }

    #[test]
    fn a_mixed_case_claim_is_normalised_not_treated_as_a_new_identity() {
        let claims = Claims {
            wallet_address: "0x742d35Cc6634C0532925a3b8D31cE5bb1C6E6B22".into(),
            iat: now_secs(),
            exp: now_secs() + 3600,
        };
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret-that-is-at-least-32-bytes-long"),
        )
        .unwrap();
        let (recovered, _) = keys().verify(&token).unwrap();
        assert_eq!(recovered.as_str(), wallet().as_str());
    }

    #[test]
    fn the_authorization_scheme_is_matched_case_insensitively() {
        assert_eq!(
            bearer_token(&header("Bearer abc.def.ghi")),
            Some("abc.def.ghi")
        );
        assert_eq!(
            bearer_token(&header("bearer abc.def.ghi")),
            Some("abc.def.ghi")
        );
        assert_eq!(
            bearer_token(&header("BEARER  abc.def.ghi")),
            Some("abc.def.ghi")
        );
    }

    #[test]
    fn a_bare_token_is_still_accepted() {
        // Retained deliberately: several native clients send one.
        assert_eq!(bearer_token(&header("abc.def.ghi")), Some("abc.def.ghi"));
    }

    #[test]
    fn other_schemes_and_empty_headers_yield_nothing() {
        assert_eq!(bearer_token(&header("Basic dXNlcjpwYXNz")), None);
        assert_eq!(bearer_token(&header("Bearer   ")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn the_challenge_message_is_byte_exact() {
        let message = challenge_message(&wallet(), &"a".repeat(64));
        let expected = concat!(
            "Welcome to FruitNation!\n\n",
            "Click to sign in and accept the FruitNation Terms of Service.\n\n",
            "This request will not trigger a blockchain transaction or cost any gas fees.\n\n",
            "Wallet address:\n0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22\n\n",
            "Nonce:\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        assert_eq!(message, expected);
        assert!(!message.ends_with('\n'), "no trailing newline");
    }

    #[test]
    fn the_binding_message_is_byte_exact() {
        let binding = key_binding_message(&wallet(), "04abcdef");
        assert_eq!(
            binding,
            "FruitNation Public Key Binding\n\n\
             Address: 0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22\n\
             Encryption Public Key: 04abcdef"
        );
    }

    #[test]
    fn key_binding_verification_fails_closed_on_garbage() {
        // A malformed signature must be a refusal, never a warning.
        assert!(!verify_key_binding(&wallet(), "04ab", "0xnotasignature"));
        assert!(!verify_key_binding(&wallet(), "04ab", ""));
        assert!(!verify_key_binding(
            &wallet(),
            "04ab",
            &format!("0x{}", "0".repeat(130))
        ));
    }

    /// What this layer uniquely owns: the *shape* of a token — exactly 64
    /// lowercase hex characters — and that two successive mints differ. The
    /// generator's quality, the sixteen-draw sweep and the never-zero and
    /// never-repeat guarantees live in `core::random`'s own tests, on the
    /// `hex_32` this delegates to; re-running them here would just be a second,
    /// drifting copy of the same loop.
    #[test]
    fn a_nonce_is_64_lowercase_hex_and_fresh_each_call() {
        let a = random_hex_32().expect("the OS CSPRNG should answer");
        let b = random_hex_32().expect("the OS CSPRNG should answer");
        assert_eq!(a.len(), 64);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "two successive nonces were identical");
    }
}
