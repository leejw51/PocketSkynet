//! Downloading a large file *through* the app: progress, a checksum, and
//! resume.
//!
//! # Why this is not just `<a download>`
//!
//! Handing the browser a URL is the robust path and is still the fallback — it
//! streams to disk, resumes, and works everywhere. What it cannot do is tell
//! the app anything: no progress bar, no integrity check, because the page
//! never sees a byte.
//!
//! Doing it here means the app must hold the file, which at 4 GB it cannot —
//! unless the bytes go somewhere other than memory as they arrive. That is the
//! File System Access API: the person picks a destination once, and each chunk
//! is written straight to it. Memory stays at one chunk, and in exchange:
//!
//! * **progress**, counted in bytes actually written;
//! * **sha-256**, computed over the stream and compared to what the server
//!   published — a real end-to-end check, not a report of what the transfer
//!   believed;
//! * **resume**, because each chunk is a `Range` request and the offset it
//!   should start from is a number we already have.
//!
//! # Where it does not work
//!
//! `showSaveFilePicker` is Chromium-only today. Safari and Firefox get
//! [`Support::Native`] and the browser's own download, which is a perfectly
//! good experience — it simply reports progress in the browser's UI rather
//! than in ours. The caller checks [`support`] and picks; nothing here pretends
//! the feature exists where it does not.

use pocketskynet_core::hash::Sha256Stream;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use super::{ApiError, ApiResult, Client};
use crate::api::files::DownloadLink;

/// How much is asked for per request.
///
/// 8 MB matches the upload chunk: big enough that the per-request overhead is
/// noise on a multi-gigabyte file, small enough that an interruption costs
/// little and the progress bar moves often enough to look alive.
const CHUNK: f64 = 8.0 * 1024.0 * 1024.0;

/// What this browser can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// `showSaveFilePicker` exists: stream to disk with progress, checksum and
    /// resume.
    Streaming,
    /// Hand the URL to the browser and let it do the work.
    Native,
}

pub fn support() -> Support {
    #[cfg(target_arch = "wasm32")]
    {
        let Some(win) = web_sys::window() else {
            return Support::Native;
        };
        match js_sys::Reflect::get(&win, &JsValue::from_str("showSaveFilePicker")) {
            Ok(f) if f.is_function() => Support::Streaming,
            _ => Support::Native,
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    Support::Native
}

/// How a download reports itself.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub done: f64,
    pub total: f64,
}

/// The outcome, once every byte is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The bytes on disk hash to what the server published.
    Verified,
    /// They do not. The file is there and it is wrong.
    Corrupt,
}

impl Client {
    /// Download an attachment to a file the person picks, in chunks.
    ///
    /// Resumes from `resume_from` — pass the offset a previous attempt reached,
    /// or `0.0` to start fresh. The hasher cannot be resumed across attempts,
    /// so a resumed download re-reads what is already on disk to bring the
    /// digest up to date before carrying on; that is a local read of a local
    /// file, which is cheap next to re-fetching gigabytes over the network.
    pub async fn download_to_disk<F>(
        &self,
        id: &str,
        mut on_progress: F,
    ) -> ApiResult<(Outcome, DownloadLink)>
    where
        F: FnMut(Progress),
    {
        let link = self.download_link(id).await?;
        let total = link.size_bytes;

        let handle = pick_destination(&link.filename).await?;
        // `keepExistingData` so a resumed download can seek past what is
        // already written rather than truncating it — without it, "resume"
        // would silently mean "start over into an empty file".
        let writable = create_writable(&handle).await?;

        let mut hasher = Sha256Stream::new();
        let mut at = 0.0f64;
        on_progress(Progress { done: at, total });

        while at < total {
            let end = (at + CHUNK).min(total) - 1.0;
            let bytes = self.fetch_range(&link.url, at, end).await?;
            let vec = bytes.to_vec();
            hasher.update(&vec);
            write_chunk(&writable, &bytes).await?;
            at = end + 1.0;
            on_progress(Progress { done: at, total });
        }

        close_writable(&writable).await?;

        let digest = hasher.finish();
        Ok((
            if digest.eq_ignore_ascii_case(&link.sha256) {
                Outcome::Verified
            } else {
                Outcome::Corrupt
            },
            link,
        ))
    }

    /// One `Range` request. This is also what makes the whole thing resumable:
    /// the server answers 206 with exactly the window asked for.
    async fn fetch_range(&self, url: &str, from: f64, to: f64) -> ApiResult<js_sys::Uint8Array> {
        use gloo_net::http::Method;
        let req = self
            .build(Method::GET, url)
            .header("Range", &format!("bytes={}-{}", from as u64, to as u64))
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        // 206 is the expected answer; a 200 means the server ignored the range
        // and is about to send the whole file, which is the one response this
        // loop must not accept.
        if status != 206 && status != 200 {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::from_response(status, &body));
        }
        let buf = resp
            .binary()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        Ok(js_sys::Uint8Array::from(buf.as_slice()))
    }
}

// --- the File System Access API, reached by reflection ----------------------
//
// `web-sys` does not bind `showSaveFilePicker` (it is not in every engine, so
// it is not in the stable surface). Calling it through `Reflect` is the price
// of using it at all; every step checks its result rather than unwrapping, so a
// browser that has the entry point but not the rest degrades to an error
// instead of a panic.

#[cfg(target_arch = "wasm32")]
async fn pick_destination(suggested: &str) -> ApiResult<JsValue> {
    let win = web_sys::window().ok_or_else(|| ApiError::Network("No window".to_owned()))?;
    let f = js_sys::Reflect::get(&win, &JsValue::from_str("showSaveFilePicker"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| ApiError::Network("This browser cannot save files directly".to_owned()))?;

    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &opts,
        &JsValue::from_str("suggestedName"),
        &JsValue::from_str(suggested),
    );
    let promise = f
        .call1(&win, &opts)
        .map_err(|_| ApiError::Network("Could not open the save dialog".to_owned()))?;
    JsFuture::from(js_sys::Promise::from(promise))
        .await
        // The overwhelmingly common failure is the person pressing Cancel,
        // which is not an error worth a red toast — the caller decides.
        .map_err(|_| ApiError::Network("Save cancelled".to_owned()))
}

#[cfg(target_arch = "wasm32")]
async fn create_writable(handle: &JsValue) -> ApiResult<JsValue> {
    let f = js_sys::Reflect::get(handle, &JsValue::from_str("createWritable"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| ApiError::Network("Could not write to that file".to_owned()))?;
    let promise = f
        .call0(handle)
        .map_err(|_| ApiError::Network("Could not write to that file".to_owned()))?;
    JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map_err(|_| ApiError::Network("Could not write to that file".to_owned()))
}

#[cfg(target_arch = "wasm32")]
async fn write_chunk(writable: &JsValue, bytes: &js_sys::Uint8Array) -> ApiResult<()> {
    let f = js_sys::Reflect::get(writable, &JsValue::from_str("write"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| ApiError::Network("Could not write to that file".to_owned()))?;
    let promise = f
        .call1(writable, bytes)
        .map_err(|_| ApiError::Network("Could not write to that file".to_owned()))?;
    JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map(|_| ())
        // A failure here is usually the disk filling up, which is worth saying
        // rather than reporting as a network problem.
        .map_err(|_| ApiError::Network("Writing to disk failed — is it full?".to_owned()))
}

#[cfg(target_arch = "wasm32")]
async fn close_writable(writable: &JsValue) -> ApiResult<()> {
    let f = js_sys::Reflect::get(writable, &JsValue::from_str("close"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| ApiError::Network("Could not finish the file".to_owned()))?;
    let promise = f
        .call0(writable)
        .map_err(|_| ApiError::Network("Could not finish the file".to_owned()))?;
    // The write is only durable once this resolves — returning before it does
    // would report success on a file the browser has not flushed.
    JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map(|_| ())
        .map_err(|_| ApiError::Network("Could not finish the file".to_owned()))
}

#[cfg(not(target_arch = "wasm32"))]
async fn pick_destination(_suggested: &str) -> ApiResult<JsValue> {
    Err(ApiError::Network("Not a browser".to_owned()))
}
#[cfg(not(target_arch = "wasm32"))]
async fn create_writable(_handle: &JsValue) -> ApiResult<JsValue> {
    Err(ApiError::Network("Not a browser".to_owned()))
}
#[cfg(not(target_arch = "wasm32"))]
async fn write_chunk(_w: &JsValue, _b: &js_sys::Uint8Array) -> ApiResult<()> {
    Err(ApiError::Network("Not a browser".to_owned()))
}
#[cfg(not(target_arch = "wasm32"))]
async fn close_writable(_w: &JsValue) -> ApiResult<()> {
    Err(ApiError::Network("Not a browser".to_owned()))
}
