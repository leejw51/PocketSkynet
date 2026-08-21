//! The transport-agnostic API layer.

use http::Method;
use pocketskynet_core::wallet::Wallet;
use serde::de::DeserializeOwned;

use crate::error::ClientError;
use crate::transport::Transport;
use crate::types::{
    ChallengeRequest, ChallengeResponse, CreateRoomRequest, ErrorEnvelope, HealthResponse,
    LoginRequest, LoginResponse, Message, Room, SendMessageRequest,
};

/// A PocketSkynet API client over one [`Transport`].
///
/// Construct a transport first ([`Transport::http1`] / [`Transport::http3`]),
/// then wrap it. [`Client::login`] stores the JWT; every later call sends it
/// as `Authorization: Bearer <jwt>`.
pub struct Client {
    transport: Transport,
    token: Option<String>,
}

impl Client {
    pub fn new(transport: Transport) -> Self {
        Self {
            transport,
            token: None,
        }
    }

    /// The transport in use — `"http/1.1"` or `"h3"`.
    pub fn transport_name(&self) -> &'static str {
        self.transport.name()
    }

    /// The JWT from a successful [`Client::login`], if any.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Adopt a token obtained elsewhere (a stored session, say).
    pub fn set_token(&mut self, token: String) {
        self.token = Some(token);
    }

    // --- plumbing ----------------------------------------------------------

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        authed: bool,
        body: Option<serde_json::Value>,
    ) -> Result<T, ClientError> {
        let token = if authed {
            Some(self.token.as_deref().ok_or(ClientError::NotLoggedIn)?)
        } else {
            None
        };
        let response = self
            .transport
            .request(method, path, token, body.as_ref())
            .await?;

        if (200..300).contains(&response.status) {
            return Ok(serde_json::from_slice(&response.body)?);
        }

        // Non-success: surface the server's own words (§1.5's envelope) when
        // the body carries them, the raw bytes otherwise.
        let (message, code) = match serde_json::from_slice::<ErrorEnvelope>(&response.body) {
            Ok(envelope) => {
                let mut message = envelope.message.unwrap_or_default();
                if let Some(errors) = &envelope.errors {
                    if !errors.is_empty() {
                        message = format!("{message}: {}", errors.join("; "));
                    }
                }
                (message, envelope.code)
            }
            Err(_) => (String::from_utf8_lossy(&response.body).into_owned(), None),
        };
        Err(ClientError::Api {
            status: response.status,
            message,
            code,
        })
    }

    // --- endpoints ---------------------------------------------------------

    /// `GET /api/health` — unauthenticated, never rate-limited.
    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        self.call(Method::GET, "/api/health", false, None).await
    }

    /// `POST /api/auth/challenge` — ask for the string to sign.
    pub async fn challenge(&self, wallet_address: &str) -> Result<ChallengeResponse, ClientError> {
        let body = serde_json::to_value(ChallengeRequest {
            wallet_address: wallet_address.to_owned(),
        })?;
        self.call(Method::POST, "/api/auth/challenge", false, Some(body))
            .await
    }

    /// The full login flow: challenge → EIP-191 `personal_sign` (via
    /// `pocketskynet-core`, over the challenge string **verbatim**) → login.
    /// Stores the JWT on success.
    ///
    /// `username` is only needed the first time a wallet signs in. When it is
    /// omitted and the server answers "Username is required for first-time
    /// login", one retry is made with the deterministic protocol username for
    /// the address — a failed login burns its challenge, so the retry fetches
    /// a fresh one.
    pub async fn login(
        &mut self,
        wallet: &Wallet,
        username: Option<&str>,
    ) -> Result<LoginResponse, ClientError> {
        match self.login_once(wallet, username).await {
            Err(ClientError::Api {
                status: 400,
                message,
                ..
            }) if username.is_none() && message.contains("Username is required") => {
                let fallback = pocketskynet_core::deterministic_username(wallet.address());
                self.login_once(wallet, Some(&fallback)).await
            }
            other => other,
        }
    }

    async fn login_once(
        &mut self,
        wallet: &Wallet,
        username: Option<&str>,
    ) -> Result<LoginResponse, ClientError> {
        let address = wallet.address().as_str().to_owned();
        let challenge = self.challenge(&address).await?;
        let signature = wallet.personal_sign(&challenge.message)?;

        let body = serde_json::to_value(LoginRequest {
            wallet_address: address,
            username: username.map(str::to_owned),
            challenge_id: challenge.challenge_id,
            signature,
        })?;
        let response: LoginResponse = self
            .call(Method::POST, "/api/auth/login", false, Some(body))
            .await?;
        self.token = Some(response.token.clone());
        Ok(response)
    }

    /// `GET /api/rooms` — every room the caller is a member of.
    pub async fn rooms(&self) -> Result<Vec<Room>, ClientError> {
        self.call(Method::GET, "/api/rooms", true, None).await
    }

    /// `POST /api/rooms` — create a channel; the caller becomes member+admin.
    pub async fn create_room(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<Room, ClientError> {
        let body = serde_json::to_value(CreateRoomRequest {
            name: name.to_owned(),
            description: description.map(str::to_owned),
        })?;
        self.call(Method::POST, "/api/rooms", true, Some(body))
            .await
    }

    /// `POST /api/rooms/{roomId}/messages` — a plaintext message. The server
    /// trims `content` before storing, so it is trimmed here first and
    /// `msgHash` is the SHA-256 of exactly what is sent (protocol §13).
    pub async fn send_message(&self, room_id: &str, text: &str) -> Result<Message, ClientError> {
        let content = text.trim().to_owned();
        let msg_hash = pocketskynet_core::msg_hash_plaintext(&content);
        let body = serde_json::to_value(SendMessageRequest {
            content,
            msg_hash,
            is_encrypted: false,
        })?;
        self.call(
            Method::POST,
            &format!("/api/rooms/{room_id}/messages"),
            true,
            Some(body),
        )
        .await
    }

    /// `GET /api/rooms/{roomId}/messages` — newest `limit` messages,
    /// returned chronologically ascending.
    pub async fn messages(&self, room_id: &str, limit: u32) -> Result<Vec<Message>, ClientError> {
        self.call(
            Method::GET,
            &format!("/api/rooms/{room_id}/messages?limit={limit}"),
            true,
            None,
        )
        .await
    }
}
