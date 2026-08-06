//! Input validation, transcribed from `docs/API.md` §3.
//!
//! Two deliberate divergences from the reference implementation, both from
//! §15's recommendation list:
//!
//! * **Trim before measuring** (§15 #20). Zod ran `.min(1)` *before*
//!   `.transform(trim)`, so `"   "` passed validation and was then stored as
//!   the empty string. Every text field here is trimmed first and an empty
//!   result is rejected, which is what every caller already assumed.
//! * **Validation failures are always 400** (§15 #5, #6). The reference let a
//!   handful of handlers fall through to a 500 because their `catch` had no
//!   `ZodError` branch. A malformed request is the client's problem and must
//!   say so.
//!
//! No regex crate is used: every rule here is a character-class or length
//! check that is clearer — and faster — written out, and the SQL-keyword probe
//! is the only one that needs more than a `matches!`.

use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use pocketskynet_core::{MessageId, RoomId, WalletAddress};
use serde::de::DeserializeOwned;

use crate::error::{ApiError, ApiResult};

/// `Number.MAX_SAFE_INTEGER`. Serials and read cursors travel through
/// JavaScript clients, so anything above this cannot survive the round trip.
pub const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

/// Largest body the API accepts, matching the reference's `express.json` cap.
pub const MAX_BODY_BYTES: usize = 100 * 1024;

/// `GET /api/users/search` is capped and ordered here (§15 #19); the reference
/// returned every matching row, unordered, which `q=0x` turned into a full
/// user dump.
pub const SEARCH_LIMIT: i64 = 50;

/// Characters rejected in room names and search queries: ``<>{};"'`\``.
fn is_forbidden_markup(c: char) -> bool {
    matches!(c, '<' | '>' | '{' | '}' | ';' | '"' | '\'' | '`' | '\\')
}

/// Usernames reject the markup set *plus* the comma, because usernames are
/// interpolated into comma-joined display strings by several clients.
fn is_forbidden_username(c: char) -> bool {
    is_forbidden_markup(c) || c == ','
}

/// C0 controls plus DEL. Kept explicit rather than using `char::is_control`,
/// which also covers C1 (`\u{80}`–`\u{9f}`) and would reject text that the
/// reference accepts.
fn is_control(c: char) -> bool {
    (c as u32) <= 0x1f || c == '\u{7f}'
}

/// Detect `;` followed by a SQL verb — the reference's crude injection probe.
///
/// Kept even though every query in this server is parameterised: it costs
/// nothing, and dropping it would let a username that *looks* like an attack
/// through, which is confusing when it later shows up in a log or a report.
fn looks_like_sql_injection(s: &str) -> bool {
    const VERBS: [&str; 9] = [
        "DROP", "DELETE", "UPDATE", "INSERT", "CREATE", "ALTER", "EXEC", "UNION", "SELECT",
    ];

    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b';' {
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        for verb in VERBS {
            let end = j + verb.len();
            if end < bytes.len()
                && bytes[j..end].eq_ignore_ascii_case(verb.as_bytes())
                && bytes[end].is_ascii_whitespace()
            {
                return true;
            }
        }
    }
    false
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_any_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A required field that was absent, as a validation error.
pub fn required(field: &str, what: &str) -> ApiError {
    ApiError::field(field, &format!("{what} is required"))
}

// --------------------------------------------------------------- scalars ---

/// `walletAddress` — `0x` + 40 hex, normalised to lowercase by the newtype.
pub fn wallet_address(field: &str, raw: Option<&str>) -> ApiResult<WalletAddress> {
    let raw = raw.ok_or_else(|| ApiError::field(field, "Invalid wallet address format"))?;
    WalletAddress::new(raw).map_err(|_| ApiError::field(field, "Invalid wallet address format"))
}

/// `username` — 3–100 characters after trimming, no markup, no control
/// characters, no SQL-verb sequences. Unicode (CJK, emoji) is allowed.
pub fn username(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("username", "Username"))?;
    let trimmed = raw.trim();

    if trimmed.chars().any(is_forbidden_username) {
        return Err(ApiError::field(
            "username",
            "Username contains invalid characters",
        ));
    }
    if trimmed.chars().any(is_control) {
        return Err(ApiError::field(
            "username",
            "Username contains control characters",
        ));
    }
    if looks_like_sql_injection(trimmed) {
        return Err(ApiError::field(
            "username",
            "Username contains invalid characters",
        ));
    }

    let len = trimmed.chars().count();
    if !(3..=100).contains(&len) {
        return Err(ApiError::field(
            "username",
            "Username must be between 3 and 100 characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `profileImage` — the chosen avatar, three-valued (see
/// `db::users::update_profile`).
///
/// Only two shapes exist, and both are an allow-list rather than a length
/// check because this string is served to *other* users' clients as an image
/// source: a free-form value here would be an open redirect into every
/// member list that renders it.
///
/// * `preset:<slug>` — a portrait from the built-in gallery. The slug
///   alphabet is `[a-z0-9-]`; which slugs exist is the client's business
///   (the gallery ships with the client, not the server).
/// * `/api/images/<sha256>.<ext>` — an upload or AI generation already
///   hosted by this server. Must parse exactly like `routes::images::serve`
///   names do, so a stored value can never point outside the image store.
///
/// An empty string clears the avatar; an absent field leaves it alone.
pub fn profile_image(raw: Option<&str>) -> ApiResult<Option<Option<String>>> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Some(None));
    }

    let bad = || ApiError::field("profileImage", "Invalid profile image");

    if let Some(slug) = trimmed.strip_prefix("preset:") {
        if slug.is_empty()
            || slug.len() > 64
            || !slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(bad());
        }
        return Ok(Some(Some(trimmed.to_owned())));
    }

    if let Some(name) = trimmed.strip_prefix("/api/images/") {
        let Some((stem, ext)) = name.rsplit_once('.') else {
            return Err(bad());
        };
        if stem.len() != 64
            || !stem.bytes().all(|b| b.is_ascii_hexdigit())
            || !matches!(ext, "png" | "jpg" | "webp" | "gif")
        {
            return Err(bad());
        }
        return Ok(Some(Some(trimmed.to_owned())));
    }

    Err(bad())
}

/// `roomId` — 10–100 characters of `[A-Za-z0-9_.-]`.
///
/// [`RoomId`] is fractionally stricter (it also rejects `.`) because ids are
/// interpolated into log lines and file names. No id this server generates
/// contains a dot, so the two agree in practice; a dotted id fails here as a
/// 400 rather than surfacing later as a 500.
pub fn room_id(raw: &str) -> ApiResult<RoomId> {
    let bad = || ApiError::field("roomId", "Room ID contains invalid characters");
    let trimmed = raw.trim();
    if !(10..=100).contains(&trimmed.len()) {
        return Err(ApiError::field(
            "roomId",
            "Room ID must be between 10 and 100 characters",
        ));
    }
    RoomId::new(trimmed).map_err(|_| bad())
}

/// `messageId` — 10–100 characters of `[A-Za-z0-9_-]` (no dot).
pub fn message_id(raw: &str) -> ApiResult<MessageId> {
    let trimmed = raw.trim();
    if !(10..=100).contains(&trimmed.len()) {
        return Err(ApiError::field(
            "messageId",
            "Message ID must be between 10 and 100 characters",
        ));
    }
    MessageId::new(trimmed)
        .map_err(|_| ApiError::field("messageId", "Message ID contains invalid characters"))
}

/// `roomName` — 1–100 characters after trimming, no markup.
pub fn room_name(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("name", "Room name"))?;
    let trimmed = raw.trim();
    if trimmed.chars().any(is_forbidden_markup) {
        return Err(ApiError::field(
            "name",
            "Room name contains invalid characters",
        ));
    }
    let len = trimmed.chars().count();
    if !(1..=100).contains(&len) {
        return Err(ApiError::field(
            "name",
            "Room name must be between 1 and 100 characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `roomDescription` — optional, ≤500 characters, no markup. An empty or
/// whitespace-only description is stored as SQL `NULL`.
pub fn room_description(raw: Option<&str>) -> ApiResult<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(is_forbidden_markup) {
        return Err(ApiError::field(
            "description",
            "Room description contains invalid characters",
        ));
    }
    if trimmed.chars().count() > 500 {
        return Err(ApiError::field(
            "description",
            "Room description must be at most 500 characters",
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

/// `messageContent` — 1–5000 characters after trimming.
///
/// There is deliberately **no** content blocklist: it was bypassable, and it
/// produced false positives on base64 ciphertext, which is most of the traffic
/// in an end-to-end-encrypted room.
pub fn message_content(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("content", "Message content"))?;
    let trimmed = raw.trim();
    let len = trimmed.chars().count();
    if len == 0 {
        return Err(required("content", "Message content"));
    }
    if len > 5000 {
        return Err(ApiError::field(
            "content",
            "Message content must be at most 5000 characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `mentions` — the wallet addresses a message declares it names.
///
/// Every entry must parse as an address, because the alternative is worse than
/// a rejection: a list that silently drops what it could not read would leave
/// the sender believing they had notified somebody they had not. Membership is
/// checked later, against the room, by `db::mentions::resolve`.
///
/// The length cap is [`crate::db::mentions::MAX_MENTIONS`] — see there for why
/// a mention list is the one part of a message worth bounding.
pub fn mention_addresses(raw: Option<Vec<String>>) -> ApiResult<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    if raw.len() > crate::db::mentions::MAX_MENTIONS {
        return Err(ApiError::field(
            "mentions",
            &format!(
                "A message can mention at most {} people",
                crate::db::mentions::MAX_MENTIONS
            ),
        ));
    }
    raw.into_iter()
        .map(|entry| {
            WalletAddress::new(&entry)
                .map(|w| w.as_str().to_owned())
                .map_err(|_| ApiError::field("mentions", "Invalid wallet address format"))
        })
        .collect()
}

/// `filename` — 1–200 characters, display only, and **never** a path.
///
/// The bytes are stored under a content hash, so this string never reaches the
/// filesystem; it exists to be shown and to be downloaded as. That is precisely
/// why it still has to be scrubbed: it ends up in a `Content-Disposition`
/// header and in every client's DOM, so a separator, a `..`, a control
/// character or a quote is either a header-splitting attempt or a lie about
/// what the file is called.
///
/// Rejecting rather than sanitising is deliberate. A silently rewritten
/// filename is a file the uploader cannot recognise later.
pub fn filename(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("filename", "Filename"))?;
    let trimmed = raw.trim();
    let len = trimmed.chars().count();
    if len == 0 {
        return Err(required("filename", "Filename"));
    }
    if len > 200 {
        return Err(ApiError::field(
            "filename",
            "Filename must be at most 200 characters",
        ));
    }
    // Path separators and the parent-directory token: nothing here is ever
    // joined to a path, and a name that tries to be one is not a name.
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err(ApiError::field(
            "filename",
            "Filename must not contain path separators",
        ));
    }
    // CR/LF would split the Content-Disposition header; the rest of C0 has no
    // business in a filename either.
    if trimmed.chars().any(is_control) {
        return Err(ApiError::field(
            "filename",
            "Filename must not contain control characters",
        ));
    }
    // `"` closes the quoted-string in Content-Disposition; the markup set is
    // rejected for the same reason room names reject it.
    if trimmed.chars().any(is_forbidden_markup) {
        return Err(ApiError::field(
            "filename",
            "Filename contains invalid characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `caption` — optional, up to 1000 characters, and the searchable half of an
/// attachment: this is where the `#hashtags` live (docs/SEARCH.md §1).
///
/// Empty after trimming becomes an empty string rather than `None`: the column
/// is `NOT NULL` because "no caption" and "empty caption" are the same thing
/// for an attachment, and a nullable column would make every read branch.
pub fn caption(raw: Option<&str>) -> ApiResult<String> {
    let Some(raw) = raw else {
        return Ok(String::new());
    };
    let trimmed = raw.trim();
    if trimmed.chars().count() > 1000 {
        return Err(ApiError::field(
            "caption",
            "Caption must be at most 1000 characters",
        ));
    }
    if trimmed.chars().any(is_control) {
        return Err(ApiError::field(
            "caption",
            "Caption must not contain control characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `msgHash` — SHA-256 of the content as sent, 64 **lowercase** hex chars.
pub fn msg_hash(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("msgHash", "Message hash"))?;
    if !is_lower_hex(raw, 64) {
        return Err(ApiError::field(
            "msgHash",
            "Message hash must be 64 lowercase hex characters",
        ));
    }
    Ok(raw.to_owned())
}

/// Message-level `iv` — 32 lowercase hex chars, nullable.
///
/// Message crypto fields are lowercase-only while room-key crypto fields
/// accept mixed case; that asymmetry is in the reference schemas and clients
/// depend on both halves of it.
pub fn message_iv(raw: Option<&str>) -> ApiResult<Option<String>> {
    match raw {
        None => Ok(None),
        Some(v) if is_lower_hex(v, 32) => Ok(Some(v.to_owned())),
        Some(_) => Err(ApiError::field(
            "iv",
            "IV must be 32 lowercase hex characters",
        )),
    }
}

/// Message-level `hmac` — 64 lowercase hex chars, nullable.
pub fn message_hmac(raw: Option<&str>) -> ApiResult<Option<String>> {
    match raw {
        None => Ok(None),
        Some(v) if is_lower_hex(v, 64) => Ok(Some(v.to_owned())),
        Some(_) => Err(ApiError::field(
            "hmac",
            "HMAC must be 64 lowercase hex characters",
        )),
    }
}

/// `encVer` — 1 (legacy) or 2 (key-separated KDF + authenticated IV).
pub fn enc_ver(raw: Option<i64>, default: i64) -> ApiResult<i64> {
    match raw {
        None => Ok(default),
        Some(v) if (1..=2).contains(&v) => Ok(v),
        Some(_) => Err(ApiError::field(
            "encVer",
            "Encryption version must be 1 or 2",
        )),
    }
}

/// `keyVersion` — the room epoch a payload is sealed under.
pub fn key_version(field: &str, raw: Option<i64>, default: i64) -> ApiResult<i64> {
    match raw {
        None => Ok(default),
        Some(v) if (1..=1_000_000).contains(&v) => Ok(v),
        Some(_) => Err(ApiError::field(
            field,
            "Key version must be between 1 and 1000000",
        )),
    }
}

/// `searchQuery` — 1–100 characters after trimming, no markup.
pub fn search_query(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("q", "Search query"))?;
    let trimmed = raw.trim();
    if trimmed.chars().any(is_forbidden_markup) {
        return Err(ApiError::field(
            "q",
            "Search query contains invalid characters",
        ));
    }
    let len = trimmed.chars().count();
    if !(1..=100).contains(&len) {
        return Err(ApiError::field(
            "q",
            "Search query must be between 1 and 100 characters",
        ));
    }
    Ok(trimmed.to_owned())
}

/// `emoticonCode` — any 1–64 characters of Unicode after trimming.
pub fn emoticon_code(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| ApiError::field("emoticonCode", "Emoticon code is required"))?;
    let trimmed = raw.trim();
    let len = trimmed.chars().count();
    if len == 0 {
        return Err(ApiError::field("emoticonCode", "Emoticon code is required"));
    }
    if len > 64 {
        return Err(ApiError::field("emoticonCode", "Emoticon code too long"));
    }
    Ok(trimmed.to_owned())
}

/// A room-key crypto field: hex of an exact length, **mixed case accepted**.
pub fn room_key_hex(field: &str, raw: Option<&str>, len: usize) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| ApiError::field(field, "is required"))?;
    if !is_any_hex(raw, len) {
        return Err(ApiError::field(
            field,
            &format!("must be {len} hex characters"),
        ));
    }
    Ok(raw.to_owned())
}

/// `ephemeralPublicKey` — 1–256 hex characters, mixed case accepted.
pub fn ephemeral_public_key(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("ephemeralPublicKey", "Ephemeral public key"))?;
    let ok = (1..=256).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return Err(ApiError::field(
            "ephemeralPublicKey",
            "Ephemeral public key must be 1-256 hex characters",
        ));
    }
    Ok(raw.to_owned())
}

/// `encryptedSymmetricKey` — opaque base64 or hex, 1–1024 characters. The
/// format is not checked because only the recipient can tell whether it is
/// well formed, and guessing wrong here would lock users out of a room.
pub fn wrapped_key(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| required("encryptedSymmetricKey", "Encrypted symmetric key"))?;
    if !(1..=1024).contains(&raw.len()) {
        return Err(ApiError::field(
            "encryptedSymmetricKey",
            "Encrypted symmetric key must be 1-1024 characters",
        ));
    }
    Ok(raw.to_owned())
}

/// An E2EE public key: uncompressed secp256k1, ≤130 hex chars, no `0x`.
pub fn public_key(raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| ApiError::field("publicKey", "Public key must be hex"))?;
    let ok = !raw.is_empty() && raw.len() <= 130 && raw.bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return Err(ApiError::field("publicKey", "Public key must be hex"));
    }
    Ok(raw.to_owned())
}

/// A `0x`-prefixed signature, ≤200 characters.
pub fn signature(field: &str, raw: Option<&str>) -> ApiResult<String> {
    let raw = raw.ok_or_else(|| ApiError::field(field, "Signature must be a hex string"))?;
    let ok = raw.len() > 2
        && raw.len() <= 200
        && raw.starts_with("0x")
        && raw[2..].bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return Err(ApiError::field(field, "Signature must be a hex string"));
    }
    Ok(raw.to_owned())
}

/// `lastReadSerial` — 0 … `Number.MAX_SAFE_INTEGER`.
pub fn serial(field: &str, raw: Option<i64>) -> ApiResult<i64> {
    let v = raw.ok_or_else(|| ApiError::field(field, "Serial is required"))?;
    if !(0..=MAX_SAFE_INT).contains(&v) {
        return Err(ApiError::field(field, "Serial out of range"));
    }
    Ok(v)
}

// --------------------------------------------------------- query strings ---

/// `?since=` / `?before=` — a parse failure or an out-of-range value becomes
/// `0`, which the query layer reads as "no filter" rather than "since epoch".
/// Garbage silently disables the filter instead of failing the request; that
/// is the reference behaviour and clients rely on it.
pub fn optional_cursor(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|v| (0..=MAX_SAFE_INT).contains(v))
        .unwrap_or(0)
}

/// `?limit=` — absent or unparseable yields 50; anything else is clamped into
/// `[1, 100]`.
pub fn page_limit(raw: Option<&str>) -> i64 {
    match raw.and_then(|s| s.trim().parse::<i64>().ok()) {
        Some(n) if n >= 1 => n.min(100),
        _ => 50,
    }
}

// ------------------------------------------------------------- extractor ---

/// A JSON body extractor that reports failures in the documented envelope.
///
/// `axum::Json` answers a syntax error with 400 and a data error with 422,
/// both as plain text. Every body failure in this API is a 400 carrying
/// `{"message":"Validation failed","errors":[…]}`, so the extraction is done
/// here instead. Handlers deserialise into permissive structs (`Option<String>`
/// and friends) and run the scalar validators above, which is what lets the
/// error strings name the field and the reason rather than echoing serde.
pub struct ValidJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|e| match e.status() {
                StatusCode::PAYLOAD_TOO_LARGE => {
                    ApiError::PayloadTooLarge("Request body is too large".into())
                }
                _ => ApiError::bad_request("Could not read request body"),
            })?;

        // Several endpoints take no body at all but share a handler shape with
        // ones that do; an absent body is an empty object so the per-field
        // validators produce "X is required" rather than "expected value".
        let slice: &[u8] = if bytes.trim_ascii().is_empty() {
            b"{}"
        } else {
            &bytes
        };

        serde_json::from_slice(slice)
            .map(ValidJson)
            .map_err(|e| ApiError::Validation(vec![format!("body: {e}")]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_addresses_are_lowercased_and_shape_checked() {
        let addr = wallet_address(
            "walletAddress",
            Some("0x742d35Cc6634C0532925a3b8D31cE5bb1C6E6B22"),
        )
        .unwrap();
        assert_eq!(addr.as_str(), "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22");

        for bad in ["", "0x", "742d35cc6634c0532925a3b8d31ce5bb1c6e6b22"] {
            let err = wallet_address("walletAddress", Some(bad)).unwrap_err();
            assert!(matches!(err, ApiError::Validation(_)));
        }
    }

    #[test]
    fn whitespace_only_text_is_rejected_not_stored_as_empty() {
        // §15 #20: the reference passed `min(1)` and then trimmed to "".
        assert!(message_content(Some("   ")).is_err());
        assert!(room_name(Some(" \t ")).is_err());
        assert!(username(Some("    ")).is_err());
    }

    #[test]
    fn text_is_trimmed_before_it_is_measured() {
        assert_eq!(message_content(Some("  hi  ")).unwrap(), "hi");
        assert_eq!(room_name(Some("  Team  ")).unwrap(), "Team");
        // 100 characters plus surrounding space still fits after trimming.
        let padded = format!("  {}  ", "a".repeat(100));
        assert_eq!(room_name(Some(&padded)).unwrap().len(), 100);
    }

    #[test]
    fn usernames_allow_unicode_but_not_markup_or_sql() {
        assert_eq!(username(Some("김정환")).unwrap(), "김정환");
        assert_eq!(username(Some("alice🍎x")).unwrap(), "alice🍎x");

        assert!(username(Some("bad<script>")).is_err());
        assert!(username(Some("a,b")).is_err(), "comma is username-specific");
        assert!(username(Some("x; DROP TABLE users")).is_err());
        assert!(username(Some("bad\u{7f}name")).is_err());
        assert!(username(Some("ab")).is_err(), "3 character minimum");
    }

    #[test]
    fn sql_probe_needs_a_semicolon_and_a_trailing_space() {
        assert!(looks_like_sql_injection("x; drop table t"));
        assert!(looks_like_sql_injection("x;UNION SELECT 1"));
        // A bare verb is a perfectly good username.
        assert!(!looks_like_sql_injection("select"));
        assert!(!looks_like_sql_injection("delete;"));
    }

    #[test]
    fn room_names_permit_commas_but_not_angle_brackets() {
        assert_eq!(room_name(Some("Team, Ops")).unwrap(), "Team, Ops");
        assert!(room_name(Some("Team <b>")).is_err());
    }

    #[test]
    fn ids_enforce_their_distinct_charsets() {
        assert!(room_id("room_1749652739650_304e0eaf").is_ok());
        assert!(room_id("short").is_err(), "10 character minimum");
        assert!(room_id("room_with/slash_x").is_err());
        assert!(message_id("msg_1749652746620_4cfe").is_ok());
        assert!(
            message_id("msg.with.dots.here").is_err(),
            "no dots in msg ids"
        );
    }

    #[test]
    fn message_crypto_fields_are_lowercase_only() {
        assert!(msg_hash(Some(&"a".repeat(64))).is_ok());
        assert!(
            msg_hash(Some(&"A".repeat(64))).is_err(),
            "uppercase hex must be rejected for message fields"
        );
        assert!(message_iv(Some(&"f".repeat(32))).is_ok());
        assert!(message_iv(Some(&"F".repeat(32))).is_err());
        assert_eq!(message_iv(None).unwrap(), None);
    }

    #[test]
    fn room_key_crypto_fields_accept_mixed_case() {
        // The case asymmetry against message fields is intentional: clients
        // already publish mixed-case wraps.
        assert!(room_key_hex("encryptionIV", Some("1A2b3C4d5E6f78901234567890AbCdEf"), 32).is_ok());
        assert!(room_key_hex("encryptionIV", Some("zz"), 32).is_err());
        assert!(ephemeral_public_key(Some("04AbCd")).is_ok());
        assert!(ephemeral_public_key(Some("04xyz")).is_err());
    }

    #[test]
    fn versions_default_rather_than_fail_when_absent() {
        assert_eq!(enc_ver(None, 1).unwrap(), 1);
        assert_eq!(enc_ver(None, 2).unwrap(), 2);
        assert_eq!(enc_ver(Some(2), 1).unwrap(), 2);
        assert!(enc_ver(Some(3), 1).is_err());

        assert_eq!(key_version("keyVersion", None, 1).unwrap(), 1);
        assert!(key_version("keyVersion", Some(0), 1).is_err());
        assert!(key_version("keyVersion", Some(1_000_001), 1).is_err());
    }

    #[test]
    fn serials_reject_values_a_javascript_client_cannot_hold() {
        assert_eq!(
            serial("lastReadSerial", Some(MAX_SAFE_INT)).unwrap(),
            MAX_SAFE_INT
        );
        assert!(serial("lastReadSerial", Some(MAX_SAFE_INT + 1)).is_err());
        assert!(serial("lastReadSerial", Some(-1)).is_err());
        assert!(serial("lastReadSerial", None).is_err());
    }

    #[test]
    fn cursors_degrade_to_no_filter_instead_of_failing() {
        assert_eq!(optional_cursor(Some("1749652746620")), 1_749_652_746_620);
        assert_eq!(optional_cursor(Some("abc")), 0);
        assert_eq!(optional_cursor(Some("-5")), 0);
        assert_eq!(optional_cursor(None), 0);
    }

    #[test]
    fn page_limit_clamps_into_the_documented_range() {
        assert_eq!(page_limit(None), 50);
        assert_eq!(page_limit(Some("garbage")), 50);
        assert_eq!(page_limit(Some("0")), 50);
        assert_eq!(page_limit(Some("7")), 7);
        assert_eq!(page_limit(Some("1000")), 100);
    }

    #[test]
    fn signatures_require_the_0x_prefix() {
        assert!(signature("signature", Some("0xabcdef12")).is_ok());
        assert!(signature("signature", Some("abcdef12")).is_err());
        assert!(signature("signature", Some("0xzz")).is_err());
        assert!(signature("signature", Some(&format!("0x{}", "a".repeat(210)))).is_err());
    }

    #[test]
    fn emoticon_codes_take_any_unicode_within_the_length_bound() {
        assert_eq!(emoticon_code(Some(" 🍎 ")).unwrap(), "🍎");
        assert!(emoticon_code(Some("")).is_err());
        assert!(emoticon_code(Some(&"a".repeat(65))).is_err());
    }
}
