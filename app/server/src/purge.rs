//! Destroying a room, all the way down to the disk.
//!
//! `DELETE FROM rooms` is one statement and the cascade in `schema.sql` takes
//! the messages, keys, invitations, members and attachment *rows* with it. What
//! it cannot take is the bytes: attachments live in `data/files/`, hosted
//! pictures and videos in `data/images/`, half-finished uploads in
//! `data/uploads/`, and SQLite has no transaction that spans a filesystem.
//!
//! For an ordinary row delete that is fine, and `db/files.rs` argues why:
//! storage is content-addressed, so the same bytes may be named by a row in
//! another room, and orphans are the price of that dedupe.
//!
//! Destroying a room is the caller that cannot accept it. Someone asking for a
//! room to be destroyed is asking to be forgotten, and a promise that stops at
//! the database is not the promise they were made: the pictures would still
//! answer to anyone who kept a URL, because a content-addressed URL is a
//! capability that no longer has a room to be revoked with.
//!
//! So the shape here is gather → delete → unlink:
//!
//! 1. **Gather** every path the room could be keeping alive, while the rows
//!    that name them still exist.
//! 2. **Delete** the room and its in-flight uploads in the database.
//! 3. **Unlink** each gathered path that nothing surviving still references —
//!    another room's attachment row, a plaintext message elsewhere, an avatar,
//!    a taught note. The reference check runs *after* the delete, so the room's
//!    own rows are gone and cannot vote to keep their own bytes.
//!
//! The residue that remains is stated rather than hidden: a picture named only
//! by an *encrypted* message in another room, written before `message_media`
//! existed, is invisible to step 3 and will be unlinked. That is the deliberate
//! ranking — see `db/media.rs::is_referenced`.
//!
//! Unlink failures are counted, never fatal. The room is already gone by then;
//! turning "one file was read-only" into a 500 would tell the caller their
//! room survived, which is the one thing that is certainly untrue.

use std::path::{Path, PathBuf};

use pocketskynet_core::WalletAddress;

use crate::db::{files, media, rooms, uploads};
use crate::error::ApiResult;
use crate::AppState;

/// What a purge destroyed, for the audit line and the response.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct PurgeReport {
    /// Attachment files unlinked from `data/files/`.
    pub attachments: usize,
    /// Hosted pictures and videos unlinked from `data/images/`.
    pub media: usize,
    /// Abandoned upload sessions dropped, with their `data/uploads/` temp file.
    pub uploads: usize,
    /// Files that were named but could not be removed. Reported rather than
    /// swallowed: an operator whose disk is read-only should be able to see it
    /// in the audit log instead of wondering why the directory is not shrinking.
    pub failed: usize,
}

/// Everything the room might be keeping alive, read before it is deleted.
struct Candidates {
    stored_names: Vec<String>,
    media_names: Vec<String>,
    temp_names: Vec<String>,
}

/// Destroy a room and everything it was holding on disk.
///
/// The caller has already decided the caller *may* — this function does no
/// permission checking, exactly like `rooms::delete_room`, which it wraps.
pub async fn destroy_room(
    state: &AppState,
    room_id: &str,
    actor: Option<&WalletAddress>,
) -> ApiResult<PurgeReport> {
    let owned = room_id.to_owned();
    let candidates = state
        .db
        .call(move |conn| {
            Ok(Candidates {
                stored_names: files::stored_names_for_room(conn, &owned)?,
                media_names: media::names_for_room(conn, &owned)?,
                temp_names: uploads::for_room(conn, &owned)?
                    .into_iter()
                    .map(|s| s.temp_name)
                    .collect(),
            })
        })
        .await?;

    let owned = room_id.to_owned();
    state
        .db
        .call(move |conn| {
            // The sessions go first and outside the room delete's own
            // transaction on purpose: `upload_sessions` has no foreign key, so
            // nothing else will ever remove them, and a failure here that left
            // the room standing is recoverable in a way the reverse is not.
            for session in uploads::for_room(conn, &owned)? {
                uploads::delete(conn, &session.id)?;
            }
            rooms::delete_room(conn, &owned)
        })
        .await?;

    let mut report = PurgeReport::default();

    // One pass for both directories, after the delete, so the room's own rows
    // are no longer there to vote for keeping their own bytes. `files` is
    // answered by a row lookup, `media` by asking every place a URL can be
    // written down.
    let Candidates {
        stored_names,
        media_names,
        temp_names,
    } = candidates;
    let (orphan_files, orphan_media) = state
        .db
        .call(move |conn| {
            Ok((
                files::orphan_candidates(conn, &stored_names)?,
                media::unreferenced(conn, &media_names)?,
            ))
        })
        .await?;

    let files_dir = state.cfg.files_dir();
    for name in orphan_files {
        match unlink(&files_dir, &name).await {
            true => report.attachments += 1,
            false => report.failed += 1,
        }
    }

    let images_dir = state.cfg.images_dir();
    for name in orphan_media {
        match unlink(&images_dir, &name).await {
            true => report.media += 1,
            false => report.failed += 1,
        }
    }

    // In-flight uploads. Not content-addressed and not shared: a temp file
    // belongs to exactly one session, which has just been deleted.
    let uploads_dir = state.cfg.uploads_dir();
    for name in temp_names {
        match unlink(&uploads_dir, &name).await {
            true => report.uploads += 1,
            false => report.failed += 1,
        }
    }

    let _ = state.log.append_audit(
        "room_purged",
        actor,
        serde_json::json!({
            "roomId": room_id,
            "attachments": report.attachments,
            "media": report.media,
            "uploads": report.uploads,
            "failed": report.failed,
        }),
    );

    Ok(report)
}

/// Remove `dir/name`, where `name` must be a plain filename.
///
/// Already-gone counts as removed — the purge's promise is about the end state,
/// and a name with no file is that end state.
///
/// The component check is the last guard on the one code path in this server
/// that deletes files it was told about rather than files it was handed. Every
/// name reaching here was generated by the server (a content hash, a session
/// uuid) or validated on the way in (`validate::media_names`), so this can only
/// fire if one of those invariants has already broken — which is exactly when a
/// `..` would otherwise be walked out of the data directory. Refused and
/// counted, never followed.
async fn unlink(dir: &Path, name: &str) -> bool {
    if !is_plain_filename(name) {
        tracing::warn!(name, "purge refused a name that is not a plain filename");
        return false;
    }
    let path: PathBuf = dir.join(name);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "purge could not unlink");
            false
        }
    }
}

/// One path component, and an ordinary one: not empty, not `.` or `..`, no
/// separator, no root or prefix.
fn is_plain_filename(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::messages::NewMessage;
    use crate::db::{messages, users};
    use crate::test_support::{state, wallet};

    const ROOM: &str = "room_purge_1";
    const OTHER: &str = "room_purge_2";

    fn digest(tag: u8) -> String {
        std::iter::repeat_n(format!("{tag:02x}"), 32).collect()
    }

    /// A room with one member, and no ceremony about keys.
    async fn room(state: &AppState, id: &str, owner: &str) {
        let (id, owner) = (id.to_owned(), owner.to_owned());
        state
            .db
            .call(move |conn| {
                users::upsert_user(conn, &owner, &owner, None, None)?;
                rooms::create_room(conn, &id, "Room", None, &owner)?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// Bytes on disk plus the row that names them, the way an upload leaves it.
    async fn attach(state: &AppState, room_id: &str, id: &str, stored: &str) {
        let dir = state.cfg.files_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(stored), b"attachment").unwrap();
        let (room_id, id, stored) = (room_id.to_owned(), id.to_owned(), stored.to_owned());
        state
            .db
            .call(move |conn| {
                files::create(
                    conn,
                    files::NewFile {
                        id,
                        room_id,
                        uploader: wallet("alice").as_str().to_owned(),
                        filename: "report.pdf".into(),
                        stored_name: stored,
                        mime: "application/pdf".into(),
                        size_bytes: 10,
                        caption: String::new(),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// Hosted bytes under `data/images/`, with nothing pointing at them yet.
    fn host_media(state: &AppState, name: &str) {
        let dir = state.cfg.images_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), b"picture").unwrap();
    }

    async fn post(state: &AppState, room_id: &str, content: &str, media: Vec<String>) {
        let (room_id, content) = (room_id.to_owned(), content.to_owned());
        let encrypted = media.iter().any(|name| !content.contains(name.as_str()));
        state
            .db
            .call(move |conn| {
                messages::create_message(
                    conn,
                    NewMessage {
                        id: format!("msg_{}", uuid::Uuid::new_v4()),
                        room_id,
                        sender: wallet("alice").as_str().to_owned(),
                        content,
                        msg_hash: "a".repeat(64),
                        is_encrypted: encrypted,
                        iv: encrypted.then(|| "f".repeat(32)),
                        hmac: encrypted.then(|| "e".repeat(64)),
                        enc_ver: 1,
                        key_version: 1,
                        parent_message_id: None,
                        mentions: Vec::new(),
                        media,
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    fn exists(dir: &std::path::Path, name: &str) -> bool {
        dir.join(name).exists()
    }

    #[tokio::test]
    async fn destroying_a_room_takes_its_bytes_with_it() {
        let state = state("purge-all");
        let alice = wallet("alice").as_str().to_owned();
        room(&state, ROOM, &alice).await;

        let stored = format!("{}.pdf", digest(0x11));
        let shown = format!("{}.png", digest(0x22));
        attach(&state, ROOM, "f1", &stored).await;
        host_media(&state, &shown);
        post(&state, ROOM, &format!("look /api/images/{shown}"), vec![]).await;

        let report = destroy_room(&state, ROOM, None).await.unwrap();

        assert_eq!(report.attachments, 1);
        assert_eq!(report.media, 1);
        assert_eq!(report.failed, 0);
        assert!(!exists(&state.cfg.files_dir(), &stored));
        assert!(!exists(&state.cfg.images_dir(), &shown));

        let gone = state
            .db
            .call(|conn| rooms::get_room(conn, ROOM))
            .await
            .unwrap();
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn a_picture_only_an_encrypted_message_named_is_still_destroyed() {
        let state = state("purge-e2ee");
        let alice = wallet("alice").as_str().to_owned();
        room(&state, ROOM, &alice).await;

        // The server cannot read this message. The declaration is the only
        // thing tying the file to the room — which is the case this whole
        // table exists for.
        let shown = format!("{}.mp4", digest(0x33));
        host_media(&state, &shown);
        post(&state, ROOM, "ciphertext", vec![shown.clone()]).await;

        let report = destroy_room(&state, ROOM, None).await.unwrap();
        assert_eq!(report.media, 1);
        assert!(!exists(&state.cfg.images_dir(), &shown));
    }

    #[tokio::test]
    async fn bytes_something_else_still_needs_are_left_alone() {
        let state = state("purge-shared");
        let alice = wallet("alice").as_str().to_owned();
        room(&state, ROOM, &alice).await;
        room(&state, OTHER, &alice).await;

        // The same three files, each held by something outside the room being
        // destroyed: a second room's attachment row, a second room's message,
        // and an avatar.
        let stored = format!("{}.pdf", digest(0x44));
        let shared = format!("{}.png", digest(0x55));
        let avatar = format!("{}.png", digest(0x66));
        attach(&state, ROOM, "f1", &stored).await;
        attach(&state, OTHER, "f2", &stored).await;
        host_media(&state, &shared);
        host_media(&state, &avatar);
        post(&state, ROOM, &format!("/api/images/{shared}"), vec![]).await;
        post(&state, OTHER, &format!("/api/images/{shared}"), vec![]).await;
        post(&state, ROOM, &format!("/api/images/{avatar}"), vec![]).await;

        let avatar_url = format!("/api/images/{avatar}");
        state
            .db
            .call(move |conn| {
                users::update_profile(conn, &alice, "alice", Some(Some(avatar_url.as_str())))?;
                Ok(())
            })
            .await
            .unwrap();

        let report = destroy_room(&state, ROOM, None).await.unwrap();

        assert_eq!(report.attachments, 0, "the other room's row still names it");
        assert_eq!(
            report.media, 0,
            "a second room and an avatar still show these"
        );
        assert!(exists(&state.cfg.files_dir(), &stored));
        assert!(exists(&state.cfg.images_dir(), &shared));
        assert!(exists(&state.cfg.images_dir(), &avatar));
    }

    #[tokio::test]
    async fn an_upload_still_in_flight_is_not_left_behind() {
        let state = state("purge-uploads");
        let alice = wallet("alice");
        room(&state, ROOM, alice.as_str()).await;

        let dir = state.cfg.uploads_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let temp = format!("upload-{}", digest(0x77));
        std::fs::write(dir.join(&temp), b"half a file").unwrap();

        let owner = alice.as_str().to_owned();
        let temp_name = temp.clone();
        state
            .db
            .call(move |conn| {
                uploads::create(
                    conn,
                    uploads::NewSession {
                        id: "up1".into(),
                        owner,
                        kind: uploads::UploadKind::File,
                        room_id: Some(ROOM.into()),
                        filename: "big.mp4".into(),
                        caption: String::new(),
                        mime: "video/mp4".into(),
                        declared_size: 1024,
                        sha256: None,
                        temp_name,
                        extra: String::new(),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let report = destroy_room(&state, ROOM, None).await.unwrap();

        assert_eq!(report.uploads, 1);
        assert!(!exists(&state.cfg.uploads_dir(), &temp));
        let session = state
            .db
            .call(|conn| uploads::read(conn, "up1"))
            .await
            .unwrap();
        assert!(session.is_none(), "the row goes with the temp file");
    }

    #[test]
    fn nothing_that_is_not_a_plain_filename_is_ever_unlinked() {
        assert!(is_plain_filename(&format!("{}.png", digest(0x01))));
        assert!(is_plain_filename("upload-123"));
        // Every one of these would leave the directory it was joined onto.
        assert!(!is_plain_filename("../jwt.secret"));
        assert!(!is_plain_filename("nested/file.png"));
        assert!(!is_plain_filename("/etc/passwd"));
        assert!(!is_plain_filename(".."));
        assert!(!is_plain_filename("."));
        assert!(!is_plain_filename(""));
    }

    #[tokio::test]
    async fn a_hostile_stored_name_is_refused_rather_than_followed() {
        let state = state("purge-traversal");
        let alice = wallet("alice").as_str().to_owned();
        room(&state, ROOM, &alice).await;

        // Only reachable if an invariant upstream has already broken — the
        // stored name is server-generated — which is precisely when the purge
        // must not be the thing that acts on it. Written directly to the row
        // because no route can produce this.
        let outside = state.cfg.data_dir.join("jwt.secret");
        std::fs::write(&outside, b"do not delete me").unwrap();
        attach(&state, ROOM, "f1", "placeholder.pdf").await;
        state
            .db
            .call(|conn| {
                conn.execute(
                    "UPDATE files SET stored_name = '../jwt.secret' WHERE id = 'f1'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let report = destroy_room(&state, ROOM, None).await.unwrap();

        assert_eq!(report.attachments, 0);
        assert_eq!(report.failed, 1, "refused, and counted as such");
        assert!(outside.exists(), "the purge stays inside its directories");
    }

    #[tokio::test]
    async fn a_missing_file_is_not_a_failure() {
        let state = state("purge-missing");
        let alice = wallet("alice").as_str().to_owned();
        room(&state, ROOM, &alice).await;

        // The row says there are bytes; there are none. A purge that reported
        // this as a failure would make every already-swept room look broken.
        let stored = format!("{}.pdf", digest(0x88));
        attach(&state, ROOM, "f1", &stored).await;
        std::fs::remove_file(state.cfg.files_dir().join(&stored)).unwrap();

        let report = destroy_room(&state, ROOM, None).await.unwrap();
        assert_eq!(report.failed, 0);
        assert_eq!(report.attachments, 1);
    }
}
