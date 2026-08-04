//! Bring-your-own-key AI client (Grok, OpenAI, Anthropic, Gemini).
//!
//! Ported from the reference client's `services/ai.ts`, including its privacy
//! stance: **API keys live only in this browser's localStorage and requests go
//! directly from the user's device to the provider.** Keys and prompts never
//! touch the PocketSkynet server — with one deliberate exception: a finished
//! generation is stored on that server before it can reach a room, either as
//! *bytes* uploaded to `POST /api/images` (the reference called the same
//! endpoint; its server just never implemented it) or, when the provider
//! answers with a link instead, as a URL handed to `POST /api/images/import`
//! for the server to fetch. See [`host_generation`] for why. What crosses
//! over is the media itself — never the key, never the prompt.
//!
//! The request builders and response parsers are pure and host-tested; only
//! the `fetch` itself is wasm-gated.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The providers the assistant can talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Grok,
    OpenAi,
    Anthropic,
    Gemini,
}

impl Provider {
    pub const ALL: [Provider; 4] = [
        Provider::Grok,
        Provider::OpenAi,
        Provider::Anthropic,
        Provider::Gemini,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Grok => "Grok (xAI)",
            Provider::OpenAi => "OpenAI",
            Provider::Anthropic => "Anthropic",
            Provider::Gemini => "Google Gemini",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Provider::Grok => "grok",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Gemini => "gemini",
        }
    }

    /// The shape a key from this provider starts with — shown as a hint, not
    /// enforced: providers change their prefixes more often than their APIs.
    pub fn key_hint(self) -> &'static str {
        match self {
            Provider::Grok => "xai-…",
            Provider::OpenAi => "sk-…",
            Provider::Anthropic => "sk-ant-…",
            Provider::Gemini => "AIza…",
        }
    }

    pub fn text_model(self) -> &'static str {
        match self {
            Provider::Grok => "grok-4.20",
            Provider::OpenAi => "gpt-4o-mini",
            Provider::Anthropic => "claude-sonnet-5",
            Provider::Gemini => "gemini-flash-latest",
        }
    }

    /// `None` means the provider has no image API (Anthropic).
    pub fn image_model(self) -> Option<&'static str> {
        match self {
            Provider::Grok => Some("grok-imagine-image"),
            Provider::OpenAi => Some("gpt-image-1-mini"),
            Provider::Anthropic => None,
            Provider::Gemini => Some("gemini-2.5-flash-image"),
        }
    }

    /// `None` means the provider has no video API. Only xAI's Imagine has
    /// one that a browser can drive with a bring-your-own key today.
    pub fn video_model(self) -> Option<&'static str> {
        match self {
            Provider::Grok => Some("grok-imagine-video"),
            _ => None,
        }
    }
}

/// Persisted assistant settings (`ps-ai` in localStorage).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AiSettings {
    #[serde(default)]
    pub keys: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub text_provider: Option<Provider>,
    #[serde(default)]
    pub image_provider: Option<Provider>,
}

const KEY_AI: &str = "ps-ai";

impl AiSettings {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            LocalStorage::get(KEY_AI).unwrap_or_default()
        }
        #[cfg(not(target_arch = "wasm32"))]
        Self::default()
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            let _ = LocalStorage::set(KEY_AI, self);
        }
    }

    pub fn key_for(&self, provider: Provider) -> Option<&str> {
        self.keys
            .get(provider.id())
            .map(String::as_str)
            .filter(|k| !k.trim().is_empty())
    }

    pub fn set_key(&mut self, provider: Provider, key: &str) {
        if key.trim().is_empty() {
            self.keys.remove(provider.id());
        } else {
            self.keys
                .insert(provider.id().to_owned(), key.trim().to_owned());
        }
    }

    /// The provider to use for text, self-healing like the reference's
    /// `normalizeAISettings`: a selection without a key falls back to any
    /// provider that has one.
    pub fn text_provider(&self) -> Option<Provider> {
        self.resolve(self.text_provider, |_| true)
    }

    /// The provider to use for images (Anthropic never qualifies).
    pub fn image_provider(&self) -> Option<Provider> {
        self.resolve(self.image_provider, |p| p.image_model().is_some())
    }

    /// The provider to use for video. Shares the *image* selection rather
    /// than storing a third choice: only one provider generates video at all,
    /// so a separate picker would be a setting with one legal value.
    pub fn video_provider(&self) -> Option<Provider> {
        self.resolve(self.image_provider, |p| p.video_model().is_some())
    }

    fn resolve(
        &self,
        chosen: Option<Provider>,
        eligible: impl Fn(Provider) -> bool,
    ) -> Option<Provider> {
        if let Some(p) = chosen {
            if eligible(p) && self.key_for(p).is_some() {
                return Some(p);
            }
        }
        Provider::ALL
            .into_iter()
            .find(|&p| eligible(p) && self.key_for(p).is_some())
    }

    pub fn any_key(&self) -> bool {
        Provider::ALL.iter().any(|&p| self.key_for(p).is_some())
    }
}

/// One prepared HTTP request: URL, extra headers, JSON body.
struct Prepared {
    url: String,
    headers: Vec<(&'static str, String)>,
    body: Value,
}

/// One turn of a multi-message conversation, for [`generate_chat`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChatTurn {
    /// `true` = the user (or a tool result being fed back); `false` = the model.
    pub user: bool,
    pub content: String,
}

/// Build a multi-turn chat request. The single-turn [`text_request`] stays —
/// every assistant surface but the Banker is one-shot by design.
fn chat_request(provider: Provider, key: &str, system: &str, turns: &[ChatTurn]) -> Prepared {
    match provider {
        Provider::Grok | Provider::OpenAi => {
            let base = match provider {
                Provider::Grok => "https://api.x.ai/v1",
                _ => "https://api.openai.com/v1",
            };
            let mut messages = vec![json!({ "role": "system", "content": system })];
            messages.extend(turns.iter().map(|t| {
                json!({ "role": if t.user { "user" } else { "assistant" }, "content": t.content })
            }));
            Prepared {
                url: format!("{base}/chat/completions"),
                headers: vec![("Authorization", format!("Bearer {key}"))],
                body: json!({ "model": provider.text_model(), "messages": messages }),
            }
        }
        Provider::Anthropic => Prepared {
            url: "https://api.anthropic.com/v1/messages".to_owned(),
            headers: vec![
                ("x-api-key", key.to_owned()),
                ("anthropic-version", "2023-06-01".to_owned()),
                (
                    "anthropic-dangerous-direct-browser-access",
                    "true".to_owned(),
                ),
            ],
            body: json!({
                "model": provider.text_model(),
                // Tool loops carry balances and tx receipts back and forth —
                // give them more room than the one-shot surfaces get.
                "max_tokens": 2048,
                "system": system,
                "messages": turns.iter().map(|t| json!({
                    "role": if t.user { "user" } else { "assistant" },
                    "content": t.content,
                })).collect::<Vec<_>>(),
            }),
        },
        Provider::Gemini => Prepared {
            url: format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
                provider.text_model()
            ),
            headers: vec![("x-goog-api-key", key.to_owned())],
            body: json!({
                "systemInstruction": { "parts": [{ "text": system }] },
                "contents": turns.iter().map(|t| json!({
                    "role": if t.user { "user" } else { "model" },
                    "parts": [{ "text": t.content }],
                })).collect::<Vec<_>>(),
            }),
        },
    }
}

/// Generate the next reply in a multi-turn conversation.
pub async fn generate_chat(
    provider: Provider,
    key: &str,
    system: &str,
    turns: &[ChatTurn],
) -> Result<String, String> {
    let body = post(chat_request(provider, key, system, turns)).await?;
    parse_text(provider, &body)
}

/// Build a text-generation request for `provider`.
fn text_request(provider: Provider, key: &str, system: &str, user: &str) -> Prepared {
    match provider {
        // Grok and OpenAI share the chat-completions shape.
        Provider::Grok | Provider::OpenAi => {
            let base = match provider {
                Provider::Grok => "https://api.x.ai/v1",
                _ => "https://api.openai.com/v1",
            };
            Prepared {
                url: format!("{base}/chat/completions"),
                headers: vec![("Authorization", format!("Bearer {key}"))],
                body: json!({
                    "model": provider.text_model(),
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": user },
                    ],
                }),
            }
        }
        Provider::Anthropic => Prepared {
            url: "https://api.anthropic.com/v1/messages".to_owned(),
            headers: vec![
                ("x-api-key", key.to_owned()),
                ("anthropic-version", "2023-06-01".to_owned()),
                // Required for browser-origin requests; without it the API
                // refuses CORS outright.
                (
                    "anthropic-dangerous-direct-browser-access",
                    "true".to_owned(),
                ),
            ],
            body: json!({
                "model": provider.text_model(),
                "max_tokens": 1024,
                "system": system,
                "messages": [{ "role": "user", "content": user }],
            }),
        },
        Provider::Gemini => Prepared {
            url: format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
                provider.text_model()
            ),
            headers: vec![("x-goog-api-key", key.to_owned())],
            body: json!({
                "systemInstruction": { "parts": [{ "text": system }] },
                "contents": [{ "role": "user", "parts": [{ "text": user }] }],
            }),
        },
    }
}

/// Pull the generated text out of a provider response.
fn parse_text(provider: Provider, body: &Value) -> Result<String, String> {
    let text = match provider {
        Provider::Grok | Provider::OpenAi => body["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned),
        Provider::Anthropic => body["content"].as_array().and_then(|parts| {
            parts
                .iter()
                .find(|p| p["type"] == "text")
                .and_then(|p| p["text"].as_str())
                .map(str::to_owned)
        }),
        Provider::Gemini => body["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            }),
    };
    match text {
        Some(t) if !t.trim().is_empty() => Ok(t.trim().to_owned()),
        _ => Err(provider_error(body)
            .unwrap_or_else(|| "The provider returned an empty response.".to_owned())),
    }
}

/// Build an image-generation request. `None` if the provider cannot.
fn image_request(provider: Provider, key: &str, prompt: &str) -> Option<Prepared> {
    let model = provider.image_model()?;
    Some(match provider {
        Provider::Grok | Provider::OpenAi => {
            let base = match provider {
                Provider::Grok => "https://api.x.ai/v1",
                _ => "https://api.openai.com/v1",
            };
            let mut body = json!({ "model": model, "prompt": prompt, "n": 1 });
            if provider == Provider::OpenAi {
                body["size"] = json!("1024x1024");
            }
            // Grok defaults to answering with a URL on xAI's CDN, which
            // expires. Bytes get re-hosted on this server by every caller
            // (the same path OpenAI's b64 answers already take), so the
            // result is a permanent same-origin URL instead.
            if provider == Provider::Grok {
                body["response_format"] = json!("b64_json");
            }
            Prepared {
                url: format!("{base}/images/generations"),
                headers: vec![("Authorization", format!("Bearer {key}"))],
                body,
            }
        }
        Provider::Gemini => Prepared {
            url: format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
            ),
            headers: vec![("x-goog-api-key", key.to_owned())],
            body: json!({ "contents": [{ "role": "user", "parts": [{ "text": prompt }] }] }),
        },
        Provider::Anthropic => unreachable!("image_model() is None"),
    })
}

/// A generated image: either already hosted by the provider, or raw bytes
/// that need hosting before they can be pasted into a room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageOut {
    Url(String),
    Bytes { mime: String, bytes: Vec<u8> },
}

/// Pull the image out of a provider response.
fn parse_image(provider: Provider, body: &Value) -> Result<ImageOut, String> {
    let missing =
        || provider_error(body).unwrap_or_else(|| "The provider returned no image.".to_owned());
    match provider {
        Provider::Grok | Provider::OpenAi => {
            let first = &body["data"][0];
            if let Some(url) = first["url"].as_str() {
                return Ok(ImageOut::Url(url.to_owned()));
            }
            if let Some(b64) = first["b64_json"].as_str() {
                let bytes = decode_base64(b64)?;
                return Ok(ImageOut::Bytes {
                    mime: "image/png".to_owned(),
                    bytes,
                });
            }
            Err(missing())
        }
        Provider::Gemini => {
            let parts = body["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for part in &parts {
                let inline = &part["inlineData"];
                if let (Some(mime), Some(data)) =
                    (inline["mimeType"].as_str(), inline["data"].as_str())
                {
                    return Ok(ImageOut::Bytes {
                        mime: mime.to_owned(),
                        bytes: decode_base64(data)?,
                    });
                }
            }
            Err(missing())
        }
        Provider::Anthropic => Err("Anthropic has no image API.".to_owned()),
    }
}

/// How long a generated clip runs, in seconds. The provider bills per second
/// and allows 1–15; six is long enough to read as a shot rather than a
/// flicker, and short enough that a mistyped prompt is not an expensive one.
const VIDEO_SECONDS: u32 = 6;

/// Build the request that *starts* a video generation. `None` if the provider
/// cannot make video.
///
/// Video is asynchronous everywhere it exists: this call returns an id, and
/// [`video_poll_request`] asks about it until the clip is rendered.
fn video_request(provider: Provider, key: &str, prompt: &str) -> Option<Prepared> {
    let model = provider.video_model()?;
    match provider {
        Provider::Grok => Some(Prepared {
            url: "https://api.x.ai/v1/videos/generations".to_owned(),
            headers: vec![("Authorization", format!("Bearer {key}"))],
            body: json!({
                "model": model,
                "prompt": prompt,
                "duration": VIDEO_SECONDS,
                "aspect_ratio": "16:9",
                "resolution": "720p",
            }),
        }),
        _ => None,
    }
}

/// The poll for one in-flight generation.
fn video_poll_url(provider: Provider, request_id: &str) -> Option<String> {
    match provider {
        Provider::Grok => Some(format!("https://api.x.ai/v1/videos/{request_id}")),
        _ => None,
    }
}

/// Ids come back from the provider and go straight into a URL path, so they
/// are checked rather than trusted: a hostile or corrupted id must not be
/// able to steer the poll at some other endpoint.
fn is_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where an in-flight video generation stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoStatus {
    /// Still rendering — poll again.
    Pending,
    /// Done, at the provider's own **temporary** URL. Callers re-host it
    /// before it reaches a room; see `api::Client::import_media`.
    Ready(String),
}

fn parse_video_start(body: &Value) -> Result<String, String> {
    match body["request_id"].as_str() {
        Some(id) if is_request_id(id) => Ok(id.to_owned()),
        _ => Err(provider_error(body)
            .unwrap_or_else(|| "The provider did not start a video generation.".to_owned())),
    }
}

fn parse_video_status(body: &Value) -> Result<VideoStatus, String> {
    match body["status"].as_str().unwrap_or_default() {
        "done" => match body["video"]["url"].as_str() {
            Some(url) if url.starts_with("https://") => Ok(VideoStatus::Ready(url.to_owned())),
            _ => Err("The provider reported a finished video with no URL.".to_owned()),
        },
        // `expired` is the provider dropping the *request*, not the clip: it
        // is as terminal as a failure and has to stop the polling loop.
        "failed" | "expired" => Err(provider_error(body)
            .unwrap_or_else(|| "The provider could not generate that video.".to_owned())),
        // An unrecognised status is treated as "still working": a provider
        // adding a `queued` state must not read as an error here.
        _ => Ok(VideoStatus::Pending),
    }
}

fn decode_base64(b64: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("bad base64 image: {e}"))
}

/// A best-effort human-readable error from any provider's error envelope.
fn provider_error(body: &Value) -> Option<String> {
    for path in [&body["error"]["message"], &body["error"], &body["message"]] {
        if let Some(s) = path.as_str() {
            return Some(s.to_owned());
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
async fn post(prepared: Prepared) -> Result<Value, String> {
    let mut req = gloo_net::http::Request::post(&prepared.url);
    for (name, value) in &prepared.headers {
        req = req.header(name, value);
    }
    let resp = req
        .json(&prepared.body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| json!({ "message": format!("HTTP {status}") }));
    if !(200..300).contains(&status) {
        return Err(provider_error(&body)
            .unwrap_or_else(|| format!("The provider answered HTTP {status}.")));
    }
    Ok(body)
}

#[cfg(not(target_arch = "wasm32"))]
async fn post(_prepared: Prepared) -> Result<Value, String> {
    Err("AI requests are wasm-only".to_owned())
}

/// The polling half of video generation: same error handling as [`post`],
/// no body.
#[cfg(target_arch = "wasm32")]
async fn get(url: &str, headers: &[(&'static str, String)]) -> Result<Value, String> {
    let mut req = gloo_net::http::Request::get(url);
    for (name, value) in headers {
        req = req.header(name, value);
    }
    let resp = req.send().await.map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| json!({ "message": format!("HTTP {status}") }));
    if !(200..300).contains(&status) {
        return Err(provider_error(&body)
            .unwrap_or_else(|| format!("The provider answered HTTP {status}.")));
    }
    Ok(body)
}

#[cfg(not(target_arch = "wasm32"))]
async fn get(_url: &str, _headers: &[(&'static str, String)]) -> Result<Value, String> {
    Err("AI requests are wasm-only".to_owned())
}

/// Generate text. `system` frames the task; `user` carries the prompt (and
/// any conversation context the caller chose to include).
pub async fn generate_text(
    provider: Provider,
    key: &str,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let body = post(text_request(provider, key, system, user)).await?;
    parse_text(provider, &body)
}

/// Generate an image for `prompt`.
pub async fn generate_image(
    provider: Provider,
    key: &str,
    prompt: &str,
) -> Result<ImageOut, String> {
    let prepared =
        image_request(provider, key, prompt).ok_or("This provider cannot generate images.")?;
    let body = post(prepared).await?;
    parse_image(provider, &body)
}

/// Start a video generation for `prompt`, returning the id to poll.
pub async fn start_video(provider: Provider, key: &str, prompt: &str) -> Result<String, String> {
    let prepared =
        video_request(provider, key, prompt).ok_or("This provider cannot generate video.")?;
    let body = post(prepared).await?;
    parse_video_start(&body)
}

/// Ask once whether a started generation has finished.
pub async fn poll_video(
    provider: Provider,
    key: &str,
    request_id: &str,
) -> Result<VideoStatus, String> {
    if !is_request_id(request_id) {
        return Err("The provider returned an unusable generation id.".to_owned());
    }
    let url = video_poll_url(provider, request_id).ok_or("This provider cannot generate video.")?;
    let body = get(&url, &[("Authorization", format!("Bearer {key}"))]).await?;
    parse_video_status(&body)
}

/// Turn whatever a provider answered with into a URL **this** server hosts.
///
/// The single rule, in one place: a generation is stored here before it can
/// reach a room. Bytes are uploaded; a provider URL is handed to the server
/// to fetch, because those links expire within about a day and their CDNs
/// send no CORS headers for the browser to read the bytes itself.
pub async fn host_generation(client: &crate::api::Client, out: ImageOut) -> Result<String, String> {
    match out {
        ImageOut::Bytes { mime, bytes } => client
            .upload_image(&mime, bytes)
            .await
            .map_err(|e| e.user_message()),
        ImageOut::Url(url) => client
            .import_media(&url)
            .await
            .map_err(|e| e.user_message()),
    }
}

/// The connectivity test behind the Keys tab's "Test" button: a one-word
/// prompt, so it proves the key works without burning tokens.
pub async fn test_key(provider: Provider, key: &str) -> Result<String, String> {
    generate_text(
        provider,
        key,
        "You are a connectivity test. Reply with the single word: pong",
        "ping",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_completion_providers_share_the_openai_shape() {
        for (provider, host) in [
            (Provider::Grok, "https://api.x.ai/v1/chat/completions"),
            (
                Provider::OpenAi,
                "https://api.openai.com/v1/chat/completions",
            ),
        ] {
            let p = text_request(provider, "k", "sys", "hi");
            assert_eq!(p.url, host);
            assert_eq!(p.headers, vec![("Authorization", "Bearer k".to_owned())]);
            assert_eq!(p.body["messages"][0]["role"], "system");
            assert_eq!(p.body["messages"][1]["content"], "hi");
        }
    }

    #[test]
    fn anthropic_sends_the_browser_access_header_and_system_field() {
        let p = text_request(Provider::Anthropic, "k", "sys", "hi");
        assert!(p
            .headers
            .iter()
            .any(|(n, v)| *n == "anthropic-dangerous-direct-browser-access" && v == "true"));
        assert_eq!(p.body["system"], "sys");
        assert_eq!(p.body["max_tokens"], 1024);
    }

    #[test]
    fn gemini_uses_system_instruction_and_key_header() {
        let p = text_request(Provider::Gemini, "AIzaX", "sys", "hi");
        assert!(p.url.contains(":generateContent"));
        assert_eq!(p.headers, vec![("x-goog-api-key", "AIzaX".to_owned())]);
        assert_eq!(p.body["systemInstruction"]["parts"][0]["text"], "sys");
    }

    #[test]
    fn text_parsing_handles_each_provider_shape() {
        let openai = json!({ "choices": [{ "message": { "content": " hello " } }] });
        assert_eq!(parse_text(Provider::Grok, &openai).unwrap(), "hello");

        let anthropic = json!({ "content": [
            { "type": "thinking", "thinking": "…" },
            { "type": "text", "text": "hi" },
        ] });
        assert_eq!(parse_text(Provider::Anthropic, &anthropic).unwrap(), "hi");

        let gemini = json!({ "candidates": [{ "content": { "parts": [
            { "text": "a" }, { "text": "b" },
        ] } }] });
        assert_eq!(parse_text(Provider::Gemini, &gemini).unwrap(), "ab");
    }

    #[test]
    fn an_error_envelope_beats_a_generic_empty_message() {
        let err = json!({ "error": { "message": "invalid api key" } });
        assert_eq!(
            parse_text(Provider::Grok, &err).unwrap_err(),
            "invalid api key"
        );
    }

    #[test]
    fn image_parsing_prefers_url_and_falls_back_to_bytes() {
        let url = json!({ "data": [{ "url": "https://img.example/a.jpg" }] });
        assert_eq!(
            parse_image(Provider::Grok, &url).unwrap(),
            ImageOut::Url("https://img.example/a.jpg".to_owned())
        );

        // "AQID" = [1, 2, 3]
        let b64 = json!({ "data": [{ "b64_json": "AQID" }] });
        assert_eq!(
            parse_image(Provider::OpenAi, &b64).unwrap(),
            ImageOut::Bytes {
                mime: "image/png".to_owned(),
                bytes: vec![1, 2, 3],
            }
        );

        let gemini = json!({ "candidates": [{ "content": { "parts": [
            { "inlineData": { "mimeType": "image/webp", "data": "AQID" } },
        ] } }] });
        assert_eq!(
            parse_image(Provider::Gemini, &gemini).unwrap(),
            ImageOut::Bytes {
                mime: "image/webp".to_owned(),
                bytes: vec![1, 2, 3],
            }
        );
    }

    #[test]
    fn settings_self_heal_to_a_provider_that_has_a_key() {
        let mut s = AiSettings::default();
        assert_eq!(s.text_provider(), None);
        assert!(!s.any_key());

        s.set_key(Provider::Anthropic, "sk-ant-x");
        // Chosen provider has no key → falls back to the one that does.
        s.text_provider = Some(Provider::Grok);
        assert_eq!(s.text_provider(), Some(Provider::Anthropic));
        // Anthropic can never serve images, even as a fallback.
        assert_eq!(s.image_provider(), None);

        s.set_key(Provider::Gemini, "AIzaY");
        assert_eq!(s.image_provider(), Some(Provider::Gemini));

        // Clearing a key removes it entirely.
        s.set_key(Provider::Anthropic, "  ");
        assert_eq!(s.key_for(Provider::Anthropic), None);
        assert_eq!(s.text_provider(), Some(Provider::Gemini));
    }

    #[test]
    fn chat_requests_carry_role_history_for_every_provider() {
        let turns = vec![
            ChatTurn {
                user: true,
                content: "hi".into(),
            },
            ChatTurn {
                user: false,
                content: "{\"tool\":\"x\"}".into(),
            },
            ChatTurn {
                user: true,
                content: "[TOOL RESULT x] ok".into(),
            },
        ];

        let p = chat_request(Provider::Grok, "k", "sys", &turns);
        assert_eq!(p.body["messages"][0]["role"], "system");
        assert_eq!(p.body["messages"][2]["role"], "assistant");
        assert_eq!(p.body["messages"][3]["content"], "[TOOL RESULT x] ok");

        let p = chat_request(Provider::Anthropic, "k", "sys", &turns);
        assert_eq!(p.body["system"], "sys");
        assert_eq!(p.body["messages"][1]["role"], "assistant");
        assert_eq!(p.body["max_tokens"], 2048);

        let p = chat_request(Provider::Gemini, "k", "sys", &turns);
        assert_eq!(p.body["contents"][1]["role"], "model");
        assert_eq!(
            p.body["contents"][2]["parts"][0]["text"],
            "[TOOL RESULT x] ok"
        );
    }

    #[test]
    fn anthropic_has_no_image_request() {
        assert!(image_request(Provider::Anthropic, "k", "p").is_none());
        assert!(image_request(Provider::Grok, "k", "p").is_some());
    }

    #[test]
    fn only_grok_generates_video_and_it_asks_for_a_bounded_clip() {
        for p in [Provider::OpenAi, Provider::Anthropic, Provider::Gemini] {
            assert!(video_request(p, "k", "a cat").is_none(), "{p:?}");
        }
        let p = video_request(Provider::Grok, "k", "a cat").unwrap();
        assert_eq!(p.url, "https://api.x.ai/v1/videos/generations");
        assert_eq!(p.body["model"], "grok-imagine-video");
        assert_eq!(p.body["prompt"], "a cat");
        // Billed per second, so the duration is ours to state, not the
        // provider's to default.
        assert_eq!(p.body["duration"], VIDEO_SECONDS);
    }

    #[test]
    fn a_started_generation_yields_an_id_that_is_safe_in_a_url() {
        let ok = json!({ "request_id": "d97415a1-5796-b7ec-379f-4e6819e08fdf" });
        assert_eq!(
            parse_video_start(&ok).unwrap(),
            "d97415a1-5796-b7ec-379f-4e6819e08fdf"
        );
        assert_eq!(
            video_poll_url(Provider::Grok, "abc").unwrap(),
            "https://api.x.ai/v1/videos/abc"
        );

        // An id that could steer the poll somewhere else is refused rather
        // than pasted into a URL path.
        for hostile in ["../chat/completions", "a/b", "a?x=1", "", "a b"] {
            assert!(!is_request_id(hostile), "{hostile:?}");
            let body = json!({ "request_id": hostile });
            assert!(parse_video_start(&body).is_err(), "{hostile:?}");
        }

        // An error envelope beats the generic message.
        let err = json!({ "error": { "message": "no credits" } });
        assert_eq!(parse_video_start(&err).unwrap_err(), "no credits");
    }

    #[test]
    fn polling_distinguishes_working_from_finished_from_dead() {
        let pending = json!({ "status": "pending" });
        assert_eq!(parse_video_status(&pending).unwrap(), VideoStatus::Pending);

        // An unknown state is still "working" — a new queue status must not
        // read as a failure.
        let queued = json!({ "status": "queued" });
        assert_eq!(parse_video_status(&queued).unwrap(), VideoStatus::Pending);

        let done = json!({
            "status": "done",
            "video": { "url": "https://vidgen.x.ai/a.mp4", "duration": 6 },
        });
        assert_eq!(
            parse_video_status(&done).unwrap(),
            VideoStatus::Ready("https://vidgen.x.ai/a.mp4".to_owned())
        );

        // `expired` is as terminal as `failed`: both must stop the loop.
        for state in ["failed", "expired"] {
            let body = json!({ "status": state, "error": { "message": "moderated" } });
            assert_eq!(parse_video_status(&body).unwrap_err(), "moderated");
        }

        // Done with nothing to show is an error, not an empty success.
        let empty = json!({ "status": "done", "video": {} });
        assert!(parse_video_status(&empty).is_err());
    }

    #[test]
    fn video_falls_back_to_whichever_provider_can_actually_make_one() {
        let mut s = AiSettings::default();
        s.set_key(Provider::Gemini, "AIzaY");
        // Gemini generates images but not video.
        assert_eq!(s.image_provider(), Some(Provider::Gemini));
        assert_eq!(s.video_provider(), None);

        s.set_key(Provider::Grok, "xai-1");
        s.image_provider = Some(Provider::Gemini);
        assert_eq!(s.image_provider(), Some(Provider::Gemini));
        assert_eq!(s.video_provider(), Some(Provider::Grok));
    }

    #[test]
    fn grok_asks_for_bytes_not_an_expiring_url() {
        let p = image_request(Provider::Grok, "k", "p").unwrap();
        assert_eq!(p.body["response_format"], "b64_json");
        let p = image_request(Provider::OpenAi, "k", "p").unwrap();
        assert!(p.body.get("response_format").is_none());
    }
}
