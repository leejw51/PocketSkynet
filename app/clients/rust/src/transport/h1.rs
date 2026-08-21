//! HTTP/1.1(+TLS) via `reqwest`.

use crate::error::ClientError;
use crate::transport::{RawResponse, TransportOptions};

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
        let body = response.bytes().await?.to_vec();
        Ok(RawResponse { status, body })
    }
}
