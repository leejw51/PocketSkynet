//! Authentication and profile endpoints (API.md §6.2).

use gloo_net::http::Method;
use pocketskynet_core::WalletAddress;
use serde::Serialize;

use super::{
    ApiError, ApiResult, BlockchainInfo, Challenge, Client, LoginResponse, SaltResponse, User,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChallengeReq<'a> {
    wallet_address: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginReq<'a> {
    wallet_address: &'a str,
    username: &'a str,
    challenge_id: &'a str,
    signature: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_key_sig: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EncryptionKeyReq<'a> {
    public_key: &'a str,
    public_key_sig: &'a str,
}

#[derive(Serialize)]
struct ProfileReq<'a> {
    username: &'a str,
    /// Three-valued on the wire: omitted leaves the stored avatar alone,
    /// `""` clears it, a value sets it (API.md §6.2.7).
    #[serde(rename = "profileImage", skip_serializing_if = "Option::is_none")]
    profile_image: Option<&'a str>,
}

/// `POST /api/events/ticket` (REALTIME.md §8.1).
#[derive(serde::Deserialize)]
struct Ticket {
    ticket: String,
}

impl Client {
    /// Step 1 of sign-in. The returned `message` must be signed **verbatim** —
    /// reconstructing it locally would break the moment the server changes a
    /// character, and there is no reason to.
    ///
    /// The challenge is single-use and is burned even by a *failed* login, so
    /// every retry needs a fresh one.
    pub async fn auth_challenge(&self, address: &WalletAddress) -> ApiResult<Challenge> {
        self.send_json(
            Method::POST,
            "/api/auth/challenge",
            &ChallengeReq {
                wallet_address: address.as_str(),
            },
        )
        .await
    }

    /// Step 2. `public_key`/`public_key_sig` are sent together or not at all —
    /// the server only verifies the binding when both are present, and omitting
    /// `public_key` while sending the signature would be silently ignored.
    #[allow(clippy::too_many_arguments)]
    pub async fn auth_login(
        &self,
        address: &WalletAddress,
        username: &str,
        challenge_id: &str,
        signature: &str,
        public_key: Option<&str>,
        public_key_sig: Option<&str>,
    ) -> ApiResult<LoginResponse> {
        // Enforce the pairing here rather than trusting call sites.
        let (pk, sig) = match (public_key, public_key_sig) {
            (Some(k), Some(s)) => (Some(k), Some(s)),
            _ => (None, None),
        };
        self.send_json(
            Method::POST,
            "/api/auth/login",
            &LoginReq {
                wallet_address: address.as_str(),
                username,
                challenge_id,
                signature,
                public_key: pk,
                public_key_sig: sig,
            },
        )
        .await
    }

    /// The caller's E2EE derivation salt. A secret — it is served only to its
    /// owner, and this client never puts it anywhere shared.
    pub async fn encryption_salt(&self) -> ApiResult<String> {
        let r: SaltResponse = self.send(Method::GET, "/api/auth/encryption-salt").await?;
        Ok(r.salt)
    }

    /// Publish (or re-publish) the encryption public key with its wallet
    /// binding signature. Called on every login because the reference server
    /// wipes `public_key_sig` on any login that omits `public_key`
    /// (API.md quirk #3) — re-publishing is cheap and heals that.
    pub async fn put_encryption_key(&self, public_key: &str, sig: &str) -> ApiResult<()> {
        self.send_ok(
            Method::PUT,
            "/api/auth/encryption-key",
            &EncryptionKeyReq {
                public_key,
                public_key_sig: sig,
            },
        )
        .await
    }

    pub async fn profile(&self) -> ApiResult<User> {
        self.send(Method::GET, "/api/auth/profile").await
    }

    /// Update the caller's profile. `profile_image` follows the wire contract:
    /// `None` leaves the avatar untouched, `Some("")` clears it, `Some(v)`
    /// sets it to a `preset:<slug>` or an `/api/images/…` URL.
    pub async fn update_profile(
        &self,
        username: &str,
        profile_image: Option<&str>,
    ) -> ApiResult<User> {
        self.send_json(
            Method::PUT,
            "/api/auth/profile",
            &ProfileReq {
                username,
                profile_image,
            },
        )
        .await
    }

    /// Stateless on the server; the client discards its token regardless of the
    /// outcome, so a failure here is not surfaced.
    pub async fn logout(&self) -> ApiResult<()> {
        self.send_ok(Method::POST, "/api/auth/logout", &serde_json::json!({}))
            .await
    }

    /// A short-lived, single-use ticket for the SSE stream.
    ///
    /// `EventSource` cannot set an `Authorization` header, and putting the JWT
    /// in the URL would leak a 30-day bearer credential into every proxy log
    /// along the path. A ticket is 30 seconds of entropy, consumed on connect —
    /// worthless by the time anyone reads the log it landed in.
    pub async fn events_ticket(&self) -> ApiResult<String> {
        let t: Ticket = self
            .send_json(Method::POST, "/api/events/ticket", &serde_json::json!({}))
            .await?;
        Ok(t.ticket)
    }

    /// Chain metadata. Unauthenticated, and cheap enough to fetch on boot so
    /// the testnet ribbon and explorer links are correct from the first paint.
    pub async fn blockchain_info(&self) -> ApiResult<BlockchainInfo> {
        self.send(Method::GET, "/api/blockchain/info").await
    }

    /// Where this server is and which transport carried this call.
    ///
    /// Unauthenticated: the addresses are the ones printed on startup, and the
    /// protocol is a property of the request the caller just made.
    pub async fn server_info(&self) -> ApiResult<crate::api::ServerInfo> {
        self.send(Method::GET, "/api/server/info").await
    }

    /// The multi-chain registry for the wallet's network switcher. The wire
    /// type is `pocketskynet_core::chain::Network` itself — server and client
    /// deserialize the same struct, so the two cannot drift.
    pub async fn networks(&self) -> ApiResult<Vec<pocketskynet_core::chain::Network>> {
        self.send(Method::GET, "/api/networks").await
    }

    /// Upload raw image or video bytes (an AI generation) and get back a
    /// same-origin URL that can be pasted into a room.
    pub async fn upload_image(&self, mime: &str, bytes: Vec<u8>) -> ApiResult<String> {
        let req = self
            .build(Method::POST, "/api/images")
            .header("Content-Type", mime)
            .body(js_sys::Uint8Array::from(bytes.as_slice()))
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let hosted: Hosted = super::decode(resp).await?;
        Ok(hosted.url)
    }

    /// Re-host a provider's **temporary** media URL on this server, and get
    /// back the permanent same-origin URL.
    ///
    /// Video generation hands back a link on the provider's CDN that stops
    /// resolving within about a day, and that CDN sends no CORS headers — so
    /// this browser can play the clip but cannot read its bytes to upload
    /// them. The server does the fetch instead, against a host allow-list
    /// (`routes/images.rs::import`). Nothing but the URL crosses over: the
    /// API key stays here, as always.
    pub async fn import_media(&self, url: &str) -> ApiResult<String> {
        let hosted: Hosted = self
            .send_json(
                Method::POST,
                "/api/images/import",
                &serde_json::json!({ "url": url }),
            )
            .await?;
        Ok(hosted.url)
    }
}

/// The one-field answer both hosting endpoints give.
#[derive(serde::Deserialize)]
struct Hosted {
    url: String,
}
