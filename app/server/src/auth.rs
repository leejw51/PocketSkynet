//! Authentication: challenge messages, JWTs, and the `AuthUser` extractor.
//!
//! There is no session table and no revocation list. A JWT is a bearer
//! credential that is valid until it expires, which is why the TTL is
//! configurable and why realtime connections re-check `exp` while they are
//! open rather than only at the handshake.

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

/// Signing and verification material.
pub struct JwtKeys {
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
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

        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
            validation,
            ttl_seconds: ttl_hours.max(1) * 3600,
        }
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

/// 32 bytes of CSPRNG output as lowercase hex — the challenge nonce and the
/// SSE ticket both use this.
pub fn random_hex_32() -> String {
    let mut buf = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    hex::encode(buf)
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

    #[test]
    fn nonces_are_64_hex_characters_and_do_not_repeat() {
        let a = random_hex_32();
        let b = random_hex_32();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
