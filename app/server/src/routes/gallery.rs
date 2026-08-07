//! The room's photo gallery: everything it shows, in one list.
//!
//! A room's media lives in two stores with nothing in common but the room.
//! Attachments are rows in `files` with authenticated bytes under
//! `data/files/`; hosted media is rowless content under `data/images/`,
//! tied to the room only through `message_media`. The chat stream renders
//! both already — but finding last month's photo by scrolling a chat stream
//! is archaeology, and that is the whole case for a gallery: the same media,
//! newest first, as a grid.
//!
//! So `GET /api/rooms/{roomId}/media` is a **read-model over both stores**,
//! merged by time. It creates no table and owns no state; everything it
//! returns is derived per request from rows that already exist, which is why
//! deleting a message or an attachment removes it from the gallery with no
//! second bookkeeping.
//!
//! # Why attachment items carry a capability URL
//!
//! A grid is fetched by `<img>` tags, which cannot send an `Authorization`
//! header, and a hundred tiles must not cost a hundred `download-token`
//! round trips before the first pixel. So the listing mints each attachment's
//! capability inline — the same single-file, hour-lived token the
//! `download-token` route hands out, scoped per file, membership re-checked
//! on every fetch. A page of tokens costs microseconds to mint; the grid
//! renders with zero further API calls. Hosted items need none of this —
//! their hash-named URLs are already the capability.

use axum::extract::{Path, Query, State};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::{files, media};
use crate::error::{ApiError, ApiResult};
use crate::routes::files::{download_scope, url_escape, INLINE_MEDIA};
use crate::routes::messages::require_member;
use crate::AppState;
use pocketskynet_core::RoomId;

pub fn router() -> Router<AppState> {
    Router::new().route("/rooms/{roomId}/media", axum::routing::get(list))
}

#[derive(Debug, Deserialize)]
struct ListParams {
    /// Page size, clamped to 1..=500. Big by default because a tile is one
    /// row of metadata, not the pixels.
    limit: Option<i64>,
    /// Epoch milliseconds, exclusive: return items strictly older. The cursor
    /// for "load more" — pass the smallest `createdAtMs` seen so far. Items
    /// sharing that exact millisecond are skipped; at gallery granularity
    /// that is a corner accepted in exchange for a cursor that never
    /// duplicates under concurrent posting, which an offset would.
    before: Option<i64>,
}

/// `GET /api/rooms/{roomId}/media?limit=&before=` — member-only.
async fn list(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(room_id): Path<String>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = RoomId::new(&room_id).map_err(|_| ApiError::not_found("Room not found"))?;
    require_member(&state, &room, &caller).await?;

    let limit = params.limit.unwrap_or(200).clamp(1, 500);
    let before = params.before.unwrap_or(i64::MAX);

    // Both sources fetch a full page each: either alone could fill it, and
    // which one dominates is not knowable before the merge. One row *past*
    // the page from each, so "is there more?" is answered by what came back
    // rather than by a second query — a source that returned exactly `limit`
    // rows would otherwise be indistinguishable from one that ran dry there.
    let owned = room.as_str().to_owned();
    let (attachments, hosted) = state
        .db
        .call(move |conn| {
            let exts: Vec<&str> = INLINE_MEDIA.iter().map(|(e, _)| *e).collect();
            Ok((
                files::media_for_room(conn, &owned, before, limit + 1, &exts)?,
                media::shown_for_room(conn, &owned, before, limit + 1)?,
            ))
        })
        .await?;

    let files_dir = state.cfg.files_dir();
    let images_dir = state.cfg.images_dir();

    // (sort key, item) — merged newest-first below. Ties broken by building
    // attachments first, matching the per-source `rowid DESC` within a batch.
    let mut items: Vec<(i64, serde_json::Value)> = Vec::new();

    for file in attachments {
        let ext = ext_of(&file.stored_name);
        // Minted per item — see the module docs for why this is the design
        // and not an extravagance.
        let token = state
            .jwt
            .issue_download(&caller, &download_scope(&file.meta.id))?;
        let thumb = crate::thumbs::exists(&files_dir, &file.stored_name).then(|| {
            format!(
                "/api/files/{}/thumbnail?dl={}",
                url_escape(&file.meta.id),
                url_escape(&token)
            )
        });
        items.push((
            file.created_ms,
            serde_json::json!({
                "kind": kind_of(ext),
                "source": "attachment",
                "id": file.meta.id,
                "filename": file.meta.filename,
                "caption": file.meta.caption,
                "sender": file.meta.uploader,
                "url": format!(
                    "/api/files/{}/raw?dl={}",
                    url_escape(&file.meta.id),
                    url_escape(&token)
                ),
                "thumbUrl": thumb,
                "createdAt": file.meta.created_at,
                "createdAtMs": file.created_ms,
            }),
        ));
    }

    for shown in hosted {
        let ext = ext_of(&shown.name);
        let thumb = crate::thumbs::exists(&images_dir, &shown.name)
            .then(|| format!("/api/images/{}/thumbnail", shown.name));
        items.push((
            shown.created_ms,
            serde_json::json!({
                "kind": kind_of(ext),
                "source": "hosted",
                "name": shown.name,
                "messageId": shown.message_id,
                "sender": shown.sender,
                "url": format!("/api/images/{}", shown.name),
                "thumbUrl": thumb,
                "createdAt": crate::db::models::iso_ms(shown.created_ms),
                "createdAtMs": shown.created_ms,
            }),
        ));
    }

    // Newest first across both sources. A stable sort, so the tie order the
    // comment above promises survives it.
    items.sort_by_key(|item| std::cmp::Reverse(item.0));
    let has_more = items.len() as i64 > limit;
    items.truncate(limit as usize);

    Ok(Json(serde_json::json!({
        "items": items.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "hasMore": has_more,
    })))
}

fn ext_of(stored_name: &str) -> &str {
    stored_name.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

/// `"video"` or `"image"` — the only two kinds that can reach here, because
/// both queries filter to the extensions `INLINE_MEDIA` names.
fn kind_of(ext: &str) -> &'static str {
    if crate::thumbs::is_video(ext) {
        "video"
    } else {
        "image"
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::db::rooms;
    use crate::routes::build;
    use crate::test_support::{register, send, send_raw, state, wallet};
    use crate::AppState;

    /// A small real PNG so the image attachment grows a thumbnail.
    fn png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(300, 200, image::Rgb([200, 40, 40]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    async fn make_room(
        router: &axum::Router,
        state: &AppState,
        token: &str,
        extra: &str,
    ) -> String {
        let room = send(
            router,
            "POST",
            "/api/rooms",
            Some(token),
            Some(serde_json::json!({ "name": "Gallery" })),
        )
        .await
        .json()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        state
            .db
            .call_blocking({
                let room = room.clone();
                let extra = extra.to_owned();
                move |conn| rooms::add_member(conn, &room, &extra)
            })
            .unwrap();
        room
    }

    /// The gallery's own posts advance the clock between them so the order
    /// the test asserts is the order the timestamps encode.
    async fn tick() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    #[tokio::test]
    async fn the_gallery_merges_both_stores_newest_first_with_working_urls() {
        let state = state("gallery-union");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state.clone());
        let room = make_room(&router, &state, &alice_token, bob.as_str()).await;

        // Oldest: an image attachment, which grows a server thumbnail.
        let photo = send_raw(
            &router,
            "POST",
            &format!("/api/rooms/{room}/files?filename=photo.png"),
            Some(&alice_token),
            png(),
            "application/octet-stream",
        )
        .await;
        assert_eq!(photo.status, StatusCode::CREATED);
        tick().await;

        // Middle: a video attachment — no thumbnail until a frame is posted.
        let clip = send_raw(
            &router,
            "POST",
            &format!("/api/rooms/{room}/files?filename=clip.mp4"),
            Some(&bob_token),
            vec![0x21u8; 2048],
            "application/octet-stream",
        )
        .await;
        assert_eq!(clip.status, StatusCode::CREATED);
        tick().await;

        // Newest: a hosted image, shown by a plaintext message.
        let hosted = send_raw(
            &router,
            "POST",
            "/api/images",
            Some(&alice_token),
            png(),
            "image/png",
        )
        .await;
        let hosted_url = hosted.json()["url"].as_str().unwrap().to_owned();
        let posted = send(
            &router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(&alice_token),
            Some(serde_json::json!({
                "content": format!("look {hosted_url}"),
                "msgHash": "a".repeat(64),
            })),
        )
        .await;
        assert_eq!(posted.status, StatusCode::OK);

        // A non-media attachment must not appear at all.
        let pdf = send_raw(
            &router,
            "POST",
            &format!("/api/rooms/{room}/files?filename=report.pdf"),
            Some(&alice_token),
            vec![0x25u8; 128],
            "application/octet-stream",
        )
        .await;
        assert_eq!(pdf.status, StatusCode::CREATED);

        let listing = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/media"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(listing.status, StatusCode::OK);
        let items = listing.json()["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 3, "the PDF is not media");

        // Newest first: hosted image, then the video, then the photo.
        assert_eq!(items[0]["source"], "hosted");
        assert_eq!(items[0]["kind"], "image");
        assert_eq!(items[0]["sender"], alice.as_str());
        assert!(items[0]["messageId"].is_string());
        assert!(
            items[0]["thumbUrl"]
                .as_str()
                .unwrap()
                .ends_with("/thumbnail"),
            "a hosted image has a public thumbnail"
        );

        assert_eq!(items[1]["source"], "attachment");
        assert_eq!(items[1]["kind"], "video");
        assert_eq!(items[1]["sender"], bob.as_str());
        assert_eq!(items[1]["filename"], "clip.mp4");
        assert!(
            items[1]["thumbUrl"].is_null(),
            "no frame was posted, so no thumbnail is promised"
        );

        assert_eq!(items[2]["source"], "attachment");
        assert_eq!(items[2]["kind"], "image");
        let thumb_url = items[2]["thumbUrl"].as_str().expect("a server thumbnail");
        assert!(thumb_url.contains("?dl="), "the capability rides the URL");

        // The minted URLs must work as a bare <img> would use them: no
        // bearer header at all.
        for uri in [items[2]["url"].as_str().unwrap(), thumb_url] {
            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let response = router.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn the_gallery_is_for_members_only() {
        let state = state("gallery-member");
        let alice = wallet("alice");
        let carol = wallet("carol");
        let alice_token = register(&state, &alice, "alice");
        let carol_token = register(&state, &carol, "carol");
        let router = build(state.clone());
        let room = make_room(&router, &state, &alice_token, alice.as_str()).await;

        let refused = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/media"),
            Some(&carol_token),
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);

        let anonymous = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/media"),
            None,
            None,
        )
        .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_cursor_pages_backwards_through_time() {
        let state = state("gallery-cursor");
        let alice = wallet("alice");
        let alice_token = register(&state, &alice, "alice");
        let router = build(state.clone());
        let room = make_room(&router, &state, &alice_token, alice.as_str()).await;

        for i in 0..3u8 {
            // Distinct bytes so nothing dedupes to one stored file.
            let mut bytes = png();
            bytes.push(i);
            let up = send_raw(
                &router,
                "POST",
                &format!("/api/rooms/{room}/files?filename=p{i}.png"),
                Some(&alice_token),
                bytes,
                "application/octet-stream",
            )
            .await;
            assert_eq!(up.status, StatusCode::CREATED);
            tick().await;
        }

        let first = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/media?limit=2"),
            Some(&alice_token),
            None,
        )
        .await;
        let page = first.json();
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(page["hasMore"], true);
        assert_eq!(items[0]["filename"], "p2.png");
        assert_eq!(items[1]["filename"], "p1.png");

        let before = items[1]["createdAtMs"].as_i64().unwrap();
        let second = send(
            &router,
            "GET",
            &format!("/api/rooms/{room}/media?limit=2&before={before}"),
            Some(&alice_token),
            None,
        )
        .await;
        let page = second.json();
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(page["hasMore"], false);
        assert_eq!(items[0]["filename"], "p0.png");
    }
}
