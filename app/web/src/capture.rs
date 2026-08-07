//! Capturing a poster frame from a video the user just picked.
//!
//! The server thumbnails every *image* itself, but a video frame needs a
//! decoder, and the server deliberately carries none — ffmpeg is exactly the
//! kind of native dependency it refuses. The browser, though, has already
//! shipped every codec it plays: an offscreen `<video>` seeks a frame, a 2D
//! canvas re-draws it small, and `toBlob` hands back a JPEG to post to
//! `POST /api/files/{id}/thumbnail`. The server re-encodes whatever arrives,
//! so nothing here is trusted — it only has to be a decodable picture.
//!
//! Everything is best-effort: a codec the browser cannot play, a file with no
//! video track, a canvas tainted by a paranoid browser — every failure is
//! `None`, the upload proceeds untouched, and the room simply shows the video
//! the way it always did. A thumbnail is never worth failing an upload over.

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

/// Matches the server's `thumbs::THUMB_EDGE`: capturing larger only uploads
/// pixels the server will immediately throw away.
const EDGE: f64 = 512.0;

/// How long the whole capture may take before it is abandoned.
///
/// Seeking needs only the file's header and one keyframe from a local file,
/// so two seconds is generous — but a broken file can leave `seeked` pending
/// forever, and an upload must never hang on a thumbnail.
const TIMEOUT_MS: u32 = 10_000;

/// A JPEG poster frame from ~10% into the clip, or `None` for any reason at
/// all.
///
/// The frame is taken a beat *into* the video rather than at zero because
/// frame zero is so often a black lead-in or an encoder splash — the point of
/// a poster is to say what the clip is.
pub async fn video_frame(file: &web_sys::File) -> Option<Vec<u8>> {
    let capture = async {
        let url = web_sys::Url::create_object_url_with_blob(file).ok()?;
        let result = frame_from_url(&url).await;
        // The revoke rule from `common::object_url`: every caller owns one.
        let _ = web_sys::Url::revoke_object_url(&url);
        result
    };
    // The timeout races the capture rather than wrapping each await: it is
    // the *total* that matters to the person watching an upload button.
    match futures::future::select(
        std::pin::pin!(capture),
        std::pin::pin!(gloo_timers::future::TimeoutFuture::new(TIMEOUT_MS)),
    )
    .await
    {
        futures::future::Either::Left((frame, _)) => frame,
        futures::future::Either::Right(((), _)) => None,
    }
}

async fn frame_from_url(url: &str) -> Option<Vec<u8>> {
    let document = web_sys::window()?.document()?;
    let video: web_sys::HtmlVideoElement =
        document.create_element("video").ok()?.dyn_into().ok()?;
    // Muted and never attached to the DOM: nothing is shown, nothing sounds.
    video.set_muted(true);
    video.set_preload("metadata");
    video.set_src(url);

    // Metadata first — duration is unknown until then, and the seek target
    // depends on it.
    wait_for(&video, "loadedmetadata", "error").await?;
    let duration = video.duration();
    let target = if duration.is_finite() && duration > 0.0 {
        (duration * 0.1).min(3.0)
    } else {
        0.0
    };
    video.set_current_time(target);
    wait_for(&video, "seeked", "error").await?;

    let (w, h) = (
        f64::from(video.video_width()),
        f64::from(video.video_height()),
    );
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    // Fit inside the edge, never upscale — the server applies the same rule.
    let scale = (EDGE / w).min(EDGE / h).min(1.0);
    let (cw, ch) = ((w * scale).round().max(1.0), (h * scale).round().max(1.0));

    let canvas: web_sys::HtmlCanvasElement =
        document.create_element("canvas").ok()?.dyn_into().ok()?;
    canvas.set_width(cw as u32);
    canvas.set_height(ch as u32);
    let ctx: web_sys::CanvasRenderingContext2d = canvas.get_context("2d").ok()??.dyn_into().ok()?;
    ctx.draw_image_with_html_video_element_and_dw_and_dh(&video, 0.0, 0.0, cw, ch)
        .ok()?;

    // `toBlob` is callback-shaped; a oneshot channel makes it awaitable.
    let (tx, rx) = futures::channel::oneshot::channel::<Option<web_sys::Blob>>();
    let tx = std::cell::RefCell::new(Some(tx));
    let cb = Closure::once(move |blob: JsValue| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(blob.dyn_into::<web_sys::Blob>().ok());
        }
    });
    canvas
        .to_blob_with_type_and_encoder_options(
            cb.as_ref().unchecked_ref(),
            "image/jpeg",
            &JsValue::from_f64(0.85),
        )
        .ok()?;
    let blob = rx.await.ok()??;
    drop(cb);

    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer())
        .await
        .ok()?;
    Some(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// Resolve when `ok` fires on the element, or `None` when `fail` does.
async fn wait_for(target: &web_sys::HtmlVideoElement, ok: &str, fail: &str) -> Option<()> {
    let (tx, rx) = futures::channel::oneshot::channel::<bool>();
    let tx = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let good = {
        let tx = tx.clone();
        gloo_events::EventListener::once(target, ok.to_owned(), move |_| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(true);
            }
        })
    };
    let bad = gloo_events::EventListener::once(target, fail.to_owned(), move |_| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(false);
        }
    });
    let outcome = rx.await.unwrap_or(false);
    drop((good, bad));
    outcome.then_some(())
}

/// Is this picked file worth pointing the capture at? By declared type first,
/// by extension as the fallback — a file picked from disk sometimes arrives
/// with an empty `type`.
pub fn is_video_file(file: &web_sys::File) -> bool {
    if file.type_().starts_with("video/") {
        return true;
    }
    let name = file.name().to_ascii_lowercase();
    [".mp4", ".m4v", ".webm", ".ogv"]
        .iter()
        .any(|ext| name.ends_with(ext))
}
