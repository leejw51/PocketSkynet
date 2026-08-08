//! Thumbnails: a small JPEG sidecar beside every picture and film.
//!
//! A chat bubble showing a 12 MB photograph, and a gallery showing a hundred
//! of them, were both paying full price for every pixel — and a video bubble
//! was worse: `<video preload="metadata">` per tile, which the client's own
//! comments describe as "a room full of videos costs a room full of
//! thumbnails". So the server keeps a downscaled copy next to the original:
//! `{stem}.{ext}` gains a `{stem}.thumb.jpg` in the **same directory**.
//!
//! # Why a sidecar and not a thumbnail store
//!
//! The two media directories have opposite access rules — `data/files/` is
//! room-scoped and authenticated, `data/images/` is a public capability-URL
//! space — and a shared thumbnail directory would sit in neither regime and
//! need its own. A sidecar inherits its directory's rules by construction:
//! the thumbnail of an attachment is served like an attachment, the thumbnail
//! of a hosted image like a hosted image. It also inherits the purge for
//! free: whatever decides the original's bytes may be unlinked has, by the
//! same reasoning, decided the sidecar's may be.
//!
//! The sidecar name can never collide with an original or be served as one:
//! every route that serves stored media validates `{64 hex}.{ext}` with a
//! single dot, and `{stem}.thumb.jpg` fails that shape everywhere. No
//! uploader can create the name either — `extension_of` reduces any filename
//! to one extension of at most eight alphanumerics.
//!
//! # Where thumbnails come from
//!
//! *Images*: generated here, at upload time. This is the one place the server
//! decodes user bytes rather than treating them as opaque — a deliberate
//! narrow exception to the "a store that sniffs is a store that can be lied
//! to" rule, bounded by decode limits set before any pixel is allocated and
//! by the failure mode: bytes that do not decode simply get no thumbnail.
//! Generation never judges an upload and never fails one.
//!
//! *Videos*: captured in the browser (a `<video>` frame drawn to a canvas)
//! and posted to a thumbnail endpoint, because decoding video server-side
//! means ffmpeg or a codec stack — a heavy native dependency this project
//! deliberately does not carry. The client's bytes are **never stored
//! verbatim**: they pass through [`render`] like any image, so what lands on
//! disk is always this server's own JPEG encoding, never an uploader's
//! hand-crafted file behind an `image/jpeg` header.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage};

/// The longest edge of a generated thumbnail, in pixels.
///
/// Grid tiles render at roughly 150–260 CSS pixels; 512 covers a 2× display
/// with a margin, and a photographic 512px JPEG at quality 80 lands in the
/// tens of kilobytes — small enough that a gallery page costs less than one
/// original.
pub const THUMB_EDGE: u32 = 512;

/// JPEG quality for the encoder. 80 is where photographs stop visibly
/// degrading at thumbnail sizes; higher buys bytes, not appearance.
const JPEG_QUALITY: u8 = 80;

/// The largest declared dimension a source image may have, either axis.
///
/// This bounds a decompression bomb *before* decoding: a 100 KB PNG can
/// declare a 60000×60000 canvas and ask for gigabytes of pixel buffer. The
/// limit is enforced by the decoder from the header, so the allocation never
/// happens. 16384² is far beyond any real photograph while refusing the
/// pathological cases.
const MAX_SOURCE_EDGE: u32 = 16_384;

/// The sidecar name for a stored original — `{stem}.thumb.jpg` — or `None`
/// for a name that is not the `{64 hex}.{ext}` shape every store writes.
///
/// Derived from the *stem* alone: storage is content-addressed, so the same
/// bytes uploaded under two extensions share one thumbnail, exactly as they
/// share their pixels.
pub fn sidecar_name(stored_name: &str) -> Option<String> {
    let (stem, _ext) = stored_name.rsplit_once('.')?;
    if stem.len() != 64 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{stem}.thumb.jpg"))
}

/// The extensions [`render`] can actually decode.
///
/// A subset of what the media routes serve: `avif` is served inline but its
/// decoder is the native dependency this module exists to avoid, so an AVIF
/// attachment simply gets no server-side thumbnail — the same graceful
/// nothing as a PDF.
pub fn is_thumbable_image(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp")
}

/// The video extensions the stores serve — the set a client-captured frame
/// may be posted for.
pub fn is_video(ext: &str) -> bool {
    matches!(ext, "mp4" | "m4v" | "webm" | "ogv")
}

/// Decode, downscale and re-encode. `None` for anything that is not a
/// decodable image — which is an answer, not an error: the caller stores no
/// thumbnail and the original is served exactly as before.
///
/// Always re-encodes, even when the source is already small: the output being
/// *this server's own encoding* is what makes accepting client-captured video
/// frames sound. A crafted file that decodes survives only as its pixels.
pub fn render(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    // Limits first, decode second: the whole point is that the bomb's
    // allocation is refused from the header, not survived.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_EDGE);
    limits.max_image_height = Some(MAX_SOURCE_EDGE);
    reader.limits(limits);

    // Decode through the decoder rather than `reader.decode()`, because the
    // orientation has to be read *before* the pixels are handed over.
    //
    // A phone camera does not rotate what it recorded. It writes the sensor's
    // own pixels and adds an Exif tag saying which way up the result is, and
    // every browser applies that tag to an `<img>` for free — which is why the
    // full-size attachment always looked right and only the preview lay on its
    // side. Re-encoding drops the tag (the output here is a bare JPEG with no
    // Exif at all, deliberately: it is this server's own encoding, which is
    // what makes accepting a client-captured video frame sound). So a
    // thumbnail that does not bake the rotation into its pixels loses the
    // information entirely, and a portrait photograph is stored sideways
    // forever.
    let mut decoder = reader.into_decoder().ok()?;
    // `NoTransforms` for every format that carries no Exif, so this costs
    // nothing for a PNG and is never a reason to reject a file.
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut source = DynamicImage::from_decoder(decoder).ok()?;
    // Before scaling, not after: `thumbnail` fits inside a square bounding box
    // by aspect ratio, and a portrait image rotated afterwards would have been
    // fitted as though it were landscape.
    source.apply_orientation(orientation);

    // `thumbnail` preserves aspect ratio inside the bounding box and uses the
    // fast path for large downscales, which is the common case — but it also
    // *up*scales, and a thumbnail bigger than its original is pure waste, so
    // an image that already fits is only re-encoded.
    let scaled = if source.width() > THUMB_EDGE || source.height() > THUMB_EDGE {
        source.thumbnail(THUMB_EDGE, THUMB_EDGE)
    } else {
        source
    };
    if scaled.width() == 0 || scaled.height() == 0 {
        return None;
    }

    // JPEG has no alpha, so transparency must be composited onto something.
    // Near-black, to match the dark-first interface a tile lands on — white
    // mats would glow like lightboxes in the gallery grid.
    const MAT: u16 = 16;
    let rgba = scaled.to_rgba8();
    let mut flat = RgbImage::new(rgba.width(), rgba.height());
    for (src, dst) in rgba.pixels().zip(flat.pixels_mut()) {
        let a = u16::from(src[3]);
        for c in 0..3 {
            dst[c] = ((u16::from(src[c]) * a + MAT * (255 - a)) / 255) as u8;
        }
    }

    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut Cursor::new(&mut out), JPEG_QUALITY)
        .encode_image(&flat)
        .ok()?;
    Some(out)
}

/// [`render`] off the async runtime. Decoding a 25 MB JPEG is real CPU work,
/// and an upload handler that did it inline would stall every other request
/// on its worker for the duration.
pub async fn render_blocking(bytes: Vec<u8>) -> Option<Vec<u8>> {
    tokio::task::spawn_blocking(move || render(&bytes))
        .await
        .ok()
        .flatten()
}

/// Write a rendered thumbnail beside its original, atomically.
///
/// Same write-then-rename discipline as the stores themselves, and
/// first-writer-wins for the same reason their writes are: the name is
/// derived from the content hash of the *original*, so whoever wrote first
/// wrote a thumbnail of the same bytes.
pub async fn store(dir: &Path, stored_name: &str, jpeg: &[u8]) -> std::io::Result<()> {
    let Some(name) = sidecar_name(stored_name) else {
        return Ok(());
    };
    let path = dir.join(&name);
    if path.exists() {
        return Ok(());
    }
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, jpeg).await?;
    tokio::fs::rename(&tmp, &path).await
}

/// Generate and store a thumbnail for an image already validated and written
/// under `dir/{stored_name}` — the shared tail of every image upload path.
///
/// Infallible by design: every failure inside is a file that simply has no
/// thumbnail, which every reader already handles, so nothing here may turn a
/// stored upload into an error response.
pub async fn accompany(dir: &Path, stored_name: &str, bytes: Vec<u8>) {
    let ext = stored_name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if !is_thumbable_image(ext) || exists(dir, stored_name) {
        return;
    }
    if let Some(jpeg) = render_blocking(bytes).await {
        if let Err(e) = store(dir, stored_name, &jpeg).await {
            tracing::warn!(stored_name, error = %e, "could not store a thumbnail");
        }
    }
}

/// As [`accompany`], but for bytes that arrived in chunks and live only on
/// disk. Reads them back bounded by `cap` — a session's declared kind has
/// already been size-checked, so the bound is belt-and-braces against a file
/// grown between checks.
pub async fn accompany_file(dir: &Path, stored_name: &str, cap: u64) {
    let ext = stored_name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if !is_thumbable_image(ext) || exists(dir, stored_name) {
        return;
    }
    let path = dir.join(stored_name);
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return;
    };
    if meta.len() > cap {
        return;
    }
    if let Ok(bytes) = tokio::fs::read(&path).await {
        accompany(dir, stored_name, bytes).await;
    }
}

/// Does `dir/{stored_name}` have a sidecar? A filesystem stat, because the
/// filesystem is the record: thumbnails are as rowless as `data/images/`
/// itself, and a table would be a second copy of the truth to keep in step.
pub fn exists(dir: &Path, stored_name: &str) -> bool {
    sidecar_path(dir, stored_name).is_some_and(|p| p.exists())
}

/// The full sidecar path, when the stored name has the shape one can exist for.
pub fn sidecar_path(dir: &Path, stored_name: &str) -> Option<PathBuf> {
    Some(dir.join(sidecar_name(stored_name)?))
}

/// Unlink the sidecar of an original the purge has already unlinked. Missing
/// counts as done — the promise is about the end state.
pub async fn unlink_sidecar(dir: &Path, stored_name: &str) -> bool {
    let Some(path) = sidecar_path(dir, stored_name) else {
        return true;
    };
    match tokio::fs::remove_file(&path).await {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not unlink a thumbnail");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 transparent PNG — the same fixture `routes/images.rs` uses.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// A real image, larger than the edge cap, to prove the downscale.
    fn big_png(width: u32, height: u32) -> Vec<u8> {
        let img = RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    /// A JPEG carrying an Exif `Orientation` tag, the way a phone writes one.
    ///
    /// Built by hand rather than checked in as a fixture so the tag being
    /// tested is visible in the test: an APP1 segment spliced in directly
    /// after the SOI marker, holding a little-endian TIFF header and a
    /// single-entry IFD0 whose only tag is 0x0112.
    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let img = RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 200])
        });
        let mut plain = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut plain), image::ImageFormat::Jpeg)
            .unwrap();

        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(b"Exif\0\0");
        payload.extend_from_slice(b"II"); // little-endian
        payload.extend_from_slice(&42u16.to_le_bytes());
        payload.extend_from_slice(&8u32.to_le_bytes()); // IFD0 starts here
        payload.extend_from_slice(&1u16.to_le_bytes()); // one entry
        payload.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        payload.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        payload.extend_from_slice(&1u32.to_le_bytes()); // count
        payload.extend_from_slice(&orientation.to_le_bytes());
        payload.extend_from_slice(&[0, 0]); // pad the value to four bytes
        payload.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        let mut out = Vec::new();
        out.extend_from_slice(&plain[..2]); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]); // APP1
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&plain[2..]);
        out
    }

    #[tokio::test]
    async fn a_deleted_sidecar_is_drawn_again_from_the_original() {
        // What makes "delete the stale ones" a complete repair. Thumbnails
        // used to be generated only at upload, so a sidecar rendered by an
        // older `render` — one that ignored Exif orientation — could never be
        // corrected: deleting it left nothing, because nothing rebuilt it.
        // The serve routes now call this on a miss.
        let dir = std::env::temp_dir().join(format!("thumbs-heal-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let stored = format!("{}.png", "d".repeat(64));
        tokio::fs::write(dir.join(&stored), big_png(800, 400))
            .await
            .unwrap();

        accompany_file(&dir, &stored, 10_000_000).await;
        assert!(exists(&dir, &stored), "the first pass draws one");

        let sidecar = sidecar_path(&dir, &stored).unwrap();
        tokio::fs::remove_file(&sidecar).await.unwrap();
        assert!(!exists(&dir, &stored));

        accompany_file(&dir, &stored, 10_000_000).await;
        assert!(exists(&dir, &stored), "and a later pass draws it again");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn a_sideways_photograph_is_stood_up_before_it_is_scaled() {
        // The reported bug. A phone does not rotate what it recorded — it
        // writes the sensor's pixels and tags which way is up. A browser
        // applies that tag to the full-size image for free, which is why only
        // the preview lay on its side; re-encoding drops the tag, so the
        // rotation has to be baked into the pixels here or it is lost.
        //
        // Orientation 6 is "rotate 90° clockwise", the one a phone held
        // upright produces. A 1200×600 source must therefore come back taller
        // than it is wide.
        let jpeg = jpeg_with_orientation(1200, 600, 6);
        let out = render(&jpeg).expect("a thumbnail");
        let decoded = image::load_from_memory(&out).expect("valid JPEG out");
        assert!(
            decoded.height() > decoded.width(),
            "a rotated source must come back portrait, got {}x{}",
            decoded.width(),
            decoded.height()
        );
        // And it is still bounded, i.e. the rotation happened before the fit
        // rather than after it.
        assert!(decoded.width() <= THUMB_EDGE && decoded.height() <= THUMB_EDGE);
        assert_eq!(decoded.height(), THUMB_EDGE);
    }

    #[test]
    fn an_untagged_image_is_left_exactly_as_it_was() {
        // The other half: orientation 1 means "no transform", and a format
        // that carries no Exif at all must not pay for this. Both must keep
        // their shape.
        let tagged = render(&jpeg_with_orientation(1200, 600, 1)).expect("a thumbnail");
        let decoded = image::load_from_memory(&tagged).unwrap();
        assert!(
            decoded.width() > decoded.height(),
            "landscape must stay landscape"
        );

        let png = render(&big_png(1200, 600)).expect("a thumbnail");
        let decoded = image::load_from_memory(&png).unwrap();
        assert!(
            decoded.width() > decoded.height(),
            "a PNG has no Exif to apply"
        );
    }

    #[test]
    fn a_real_image_renders_to_a_bounded_jpeg() {
        let jpeg = render(&big_png(1600, 900)).expect("a thumbnail");
        let decoded = image::load_from_memory(&jpeg).expect("valid JPEG out");
        assert_eq!(
            image::guess_format(&jpeg).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert!(decoded.width() <= THUMB_EDGE && decoded.height() <= THUMB_EDGE);
        // Aspect preserved: 16:9 in, 16:9 out (within integer rounding).
        assert_eq!(decoded.width(), THUMB_EDGE);
        assert_eq!(decoded.height(), THUMB_EDGE * 900 / 1600);
    }

    #[test]
    fn a_tiny_image_still_becomes_a_server_encoded_jpeg() {
        // Re-encoding even when no downscale is needed is the property that
        // makes accepting client-captured video frames sound.
        let jpeg = render(PNG).expect("a thumbnail");
        assert_eq!(
            image::guess_format(&jpeg).unwrap(),
            image::ImageFormat::Jpeg
        );
    }

    #[test]
    fn bytes_that_are_not_an_image_are_an_answer_not_an_error() {
        assert!(render(b"").is_none());
        assert!(render(b"MZ this is an executable, honest").is_none());
        assert!(
            render(&[0x89, 0x50, 0x4e, 0x47, 0x00]).is_none(),
            "truncated"
        );
    }

    #[test]
    fn a_declared_bomb_is_refused_from_the_header() {
        // A PNG header declaring a canvas past the limit. The encoder will
        // not write one, so build the IHDR by hand: 60000×60000 would ask for
        // ~14 GB of RGBA. The decode must fail fast, not allocate.
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&60000u32.to_be_bytes());
        ihdr.extend_from_slice(&60000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&(ihdr.len() as u32).to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&ihdr);
        let crc = {
            // The png crate checks the CRC before the dimensions, so it has
            // to be real for the limit check to be the thing that answers.
            let mut hasher = Crc32::new();
            hasher.update(b"IHDR");
            hasher.update(&ihdr);
            hasher.finish()
        };
        png.extend_from_slice(&crc.to_be_bytes());
        assert!(render(&png).is_none());
    }

    /// CRC-32 (IEEE), bit-by-bit — ten lines beats a dependency for one test.
    struct Crc32(u32);
    impl Crc32 {
        fn new() -> Self {
            Self(0xffff_ffff)
        }
        fn update(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 ^= u32::from(b);
                for _ in 0..8 {
                    let mask = (self.0 & 1).wrapping_neg();
                    self.0 = (self.0 >> 1) ^ (0xedb8_8320 & mask);
                }
            }
        }
        fn finish(&self) -> u32 {
            !self.0
        }
    }

    #[test]
    fn transparency_is_flattened_not_errored() {
        // The 1×1 fixture is fully transparent; the output must still be a
        // decodable opaque JPEG.
        let jpeg = render(PNG).expect("alpha flattens");
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 1));
    }

    #[test]
    fn the_sidecar_name_is_derived_from_the_stem_and_only_from_a_valid_one() {
        let stem = "a".repeat(64);
        assert_eq!(
            sidecar_name(&format!("{stem}.png")).as_deref(),
            Some(format!("{stem}.thumb.jpg").as_str())
        );
        assert_eq!(
            sidecar_name(&format!("{stem}.mp4")).as_deref(),
            Some(format!("{stem}.thumb.jpg").as_str()),
            "an image and a video of the same bytes share one sidecar"
        );
        // Not the shape a store writes → no sidecar, and in particular a
        // sidecar name itself never gets a sidecar-of-a-sidecar.
        assert_eq!(sidecar_name("../jwt.secret"), None);
        assert_eq!(sidecar_name(&format!("{}.png", "a".repeat(63))), None);
        assert_eq!(sidecar_name("noext"), None);
        // `{stem}.thumb.jpg` splits to a 70-char "stem", which fails 64-hex.
        assert_eq!(sidecar_name(&format!("{stem}.thumb.jpg")), None);
    }

    /// The property the whole sidecar scheme rests on: no sidecar name can
    /// pass the `{64 hex}.{ext}` validation the serving routes apply, so a
    /// thumbnail can never be fetched as an original.
    #[test]
    fn a_sidecar_name_never_validates_as_a_stored_original() {
        let name = sidecar_name(&format!("{}.png", "b".repeat(64))).unwrap();
        let (stem, _) = name.rsplit_once('.').unwrap();
        assert_ne!(stem.len(), 64, "the .thumb infix breaks the shape");
        assert!(!crate::db::media::is_media_name(&name));
    }

    #[tokio::test]
    async fn store_and_exists_agree_and_a_second_store_is_a_no_op() {
        let dir = std::env::temp_dir().join(format!("thumbs-test-{}", uuid::Uuid::new_v4()));
        let stored = format!("{}.png", "c".repeat(64));
        assert!(!exists(&dir, &stored));

        store(&dir, &stored, b"first").await.unwrap();
        assert!(exists(&dir, &stored));
        // First writer wins: the name derives from the original's hash, so a
        // second write is the same picture again.
        store(&dir, &stored, b"second").await.unwrap();
        let on_disk = std::fs::read(dir.join(format!("{}.thumb.jpg", "c".repeat(64)))).unwrap();
        assert_eq!(on_disk, b"first");

        assert!(unlink_sidecar(&dir, &stored).await);
        assert!(!exists(&dir, &stored));
        assert!(
            unlink_sidecar(&dir, &stored).await,
            "missing counts as done"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
