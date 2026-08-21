//! HTTP/1.1(+TLS) via `reqwest`.

use futures_util::StreamExt;

use crate::error::ClientError;
use crate::transport::{RawResponse, TransportOptions, MAX_RESPONSE_BYTES};

pub struct H1Transport {
    client: reqwest::Client,
    base: url::Url,
}

impl H1Transport {
    pub fn new(base_url: &str, options: &TransportOptions) -> Result<Self, ClientError> {
        let base = url::Url::parse(base_url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        if base.host_str().is_none() {
            return Err(ClientError::InvalidUrl(format!("no host in {base_url:?}")));
        }
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(options.insecure)
            // A server that accepts the connection but never answers must not
            // hang the client forever; connect fails faster still.
            .timeout(options.request_timeout)
            .connect_timeout(options.connect_timeout)
            .build()?;
        Ok(Self { client, base })
    }

    pub async fn request(
        &self,
        method: http::Method,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<RawResponse, ClientError> {
        let url = self
            .base
            .join(path)
            .map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        let mut request = self.client.request(method, url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();

        // Stream the body so an oversized response is refused mid-transfer
        // rather than fully buffered first — `bytes()` would honour no cap.
        let mut collected = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if collected.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(ClientError::Transport(format!(
                    "response body exceeded the {MAX_RESPONSE_BYTES}-byte cap"
                )));
            }
            collected.extend_from_slice(&chunk);
        }
        Ok(RawResponse {
            status,
            body: collected,
        })
    }
}
