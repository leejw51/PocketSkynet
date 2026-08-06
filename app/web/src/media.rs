//! Which server-hosted files a message shows.
//!
//! The counterpart to `mentions.rs`, and it exists for the same reason: the
//! server can read a plaintext message and work this out for itself, and in an
//! encrypted room it holds ciphertext and never will. Without a declaration,
//! destroying an encrypted room would leave every picture posted in it sitting
//! in `data/images/`, still served to anyone who kept the URL — a room deleted
//! everywhere except the one place the bytes actually are.
//!
//! What is declared is a filename, never the message: `{sha256}.{ext}`, which
//! is what the URL already says to anyone holding it. The server checks the
//! grammar again before it stores or acts on one (`db::media`).

/// The prefix a hosted-media URL carries, relative or after the origin.
const PREFIX: &str = "/api/images/";

/// Extensions this server will serve. The same list `routes/images.rs` holds;
/// a name outside it is not media and is left alone as text.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "mp4", "webm"];

/// Is this exactly a stored media filename?
fn is_media_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    stem.len() == 64
        && stem.bytes().all(|b| b.is_ascii_hexdigit())
        && EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Every hosted file a message's text points at, in order, without repeats.
///
/// A substring scan rather than a URL parse, for the same reason the server
/// does it that way: the link arrives bare, in markdown, or with an origin in
/// front of it, and what all three have in common is the prefix followed by a
/// name whose own grammar says where it ends.
pub fn hosted_names(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let after = &rest[at + PREFIX.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
            .unwrap_or(after.len());
        let candidate = &after[..end];
        if is_media_name(candidate) && !out.iter().any(|n| n == candidate) {
            out.push(candidate.to_owned());
        }
        rest = &after[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(tag: u8) -> String {
        std::iter::repeat_n(format!("{tag:02x}"), 32).collect()
    }

    #[test]
    fn a_generated_picture_is_declared_once_however_it_was_written() {
        let png = format!("{}.png", digest(0x11));
        let mp4 = format!("{}.mp4", digest(0x22));
        let text = format!(
            "here ![it](/api/images/{png}) and again /api/images/{png}, \
             plus http://100.64.0.7:9099/api/images/{mp4}"
        );
        assert_eq!(hosted_names(&text), vec![png, mp4]);
    }

    #[test]
    fn nothing_that_is_not_a_stored_file_is_declared() {
        // A name the server would not serve, a truncated digest, and a path —
        // each would be refused by `validate::media_names`, so sending one
        // would fail the whole message rather than the picture.
        assert!(hosted_names(&format!("/api/images/{}.exe", digest(0x33))).is_empty());
        assert!(hosted_names(&format!("/api/images/{}.png", "a".repeat(63))).is_empty());
        assert!(hosted_names("/api/images/../jwt.secret.png").is_empty());
        assert!(hosted_names("ordinary text").is_empty());
    }
}
