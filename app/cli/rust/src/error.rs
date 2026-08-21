//! One error type for the whole client.

/// Everything that can go wrong between "call a method" and "typed response".
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server answered with a non-success status. `message` is the
    /// `message` field of the error envelope when the body carried one, the
    /// raw body otherwise; `code` is the machine-readable code the two 409
    /// key-conflict responses carry.
    #[error("server returned {status}: {message}")]
    Api {
        status: u16,
        message: String,
        code: Option<String>,
    },

    /// The transport failed before any HTTP status arrived: DNS, TCP/QUIC
    /// connect, TLS, or a stream torn down mid-response.
    #[error("transport error: {0}")]
    Transport(String),

    /// The response arrived but was not the JSON shape this client expects.
    #[error("unexpected response body: {0}")]
    Decode(#[from] serde_json::Error),

    /// The `--server` URL could not be parsed or is missing a host.
    #[error("invalid server URL: {0}")]
    InvalidUrl(String),

    /// Wallet construction or signing failed in `pocketskynet-core`.
    #[error("crypto error: {0:?}")]
    Crypto(pocketskynet_core::CryptoError),

    /// A method that needs a JWT was called before [`crate::Client::login`].
    #[error("not logged in — call login() first")]
    NotLoggedIn,
}

impl From<pocketskynet_core::CryptoError> for ClientError {
    fn from(e: pocketskynet_core::CryptoError) -> Self {
        ClientError::Crypto(e)
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        ClientError::Transport(e.to_string())
    }
}
