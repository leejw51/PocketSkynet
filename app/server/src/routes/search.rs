//! Search and knowledge (docs/SEARCH.md).
//!
//! Retrieval endpoints only. The server ranks — BM25 fused with local
//! embedding cosine — and returns passages with provenance; whether those
//! passages then feed a cloud model is the client's decision, made by the
//! user, per ask, on a device that holds its own keys. No AI credential
//! exists on this server.
//!
//! * `GET  /api/search?q=…`         — hybrid search over everything visible
//! * `GET  /api/search/tags`        — hashtag browse with counts
//! * `POST /api/knowledge`          — teach a note
//! * `GET  /api/knowledge`          — list notes (`?owner=me` for mine)
//! * `DELETE /api/knowledge/{id}`   — forget a note (author only)

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ApiResult};
use crate::search::store::{self, KIND_FILE, KIND_KNOWLEDGE, KIND_MESSAGE, KIND_SITE};
use crate::validate::{self, ValidJson, SEARCH_LIMIT};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(search))
        .route("/search/tags", get(tags))
        .route("/knowledge", get(list).post(teach))
        .route("/knowledge/{id}", axum::routing::delete(forget))
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
    kind: Option<String>,
    limit: Option<i64>,
}

/// `GET /api/search` — everything the caller may see, ranked.
async fn search(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Query(params): Query<SearchParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let query = params.q.unwrap_or_default();
    if query.chars().count() > 500 {
        return Err(ApiError::field("q", "Query must be at most 500 characters"));
    }
    let kind = match params.kind.as_deref() {
        None | Some("") | Some("all") => None,
        Some(k @ (KIND_MESSAGE | KIND_KNOWLEDGE | KIND_FILE | KIND_SITE)) => Some(k.to_owned()),
        Some(_) => {
            return Err(ApiError::field(
                "kind",
                "kind must be message, knowledge, file or site",
            ));
        }
    };
    let limit = params.limit.unwrap_or(20).clamp(1, SEARCH_LIMIT) as usize;

    let viewer = caller.as_str().to_owned();
    let results = state
        .db
        .call(move |conn| store::search(conn, &viewer, &query, kind.as_deref(), limit))
        .await?;
    Ok(Json(json!({ "results": results })))
}

#[derive(Debug, Deserialize)]
struct TagParams {
    limit: Option<i64>,
}

/// `GET /api/search/tags` — the caller's visible tag cloud.
async fn tags(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Query(params): Query<TagParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200) as usize;
    let viewer = caller.as_str().to_owned();
    let tags = state
        .db
        .call(move |conn| store::tag_counts(conn, &viewer, limit))
        .await?;
    let tags: Vec<serde_json::Value> = tags
        .into_iter()
        .map(|(tag, count)| json!({ "tag": tag, "count": count }))
        .collect();
    Ok(Json(json!({ "tags": tags })))
}

#[derive(Debug, Deserialize)]
struct TeachBody {
    content: Option<String>,
    #[serde(rename = "roomId")]
    room_id: Option<String>,
    #[serde(rename = "sourceMessageId")]
    source_message_id: Option<String>,
}

/// `POST /api/knowledge` — teach. The note is global on purpose: this is a
/// self-hosted shared brain, and anything taught is meant to be found.
async fn teach(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    ValidJson(body): ValidJson<TeachBody>,
) -> ApiResult<Json<store::KnowledgeNote>> {
    let content = validate::message_content(body.content.as_deref())?;
    let room_id = match body.room_id.as_deref().filter(|r| !r.is_empty()) {
        Some(raw) => Some(validate::room_id(raw)?.to_string()),
        None => None,
    };
    let source_message_id = match body.source_message_id.as_deref().filter(|m| !m.is_empty()) {
        Some(raw) => Some(validate::message_id(raw)?.to_string()),
        None => None,
    };

    let id = uuid::Uuid::new_v4().to_string();
    let owner = caller.as_str().to_owned();
    let note = state
        .db
        .call(move |conn| {
            store::teach(
                conn,
                &id,
                &owner,
                &content,
                room_id.as_deref(),
                source_message_id.as_deref(),
                now_ms(),
            )
        })
        .await?;
    Ok(Json(note))
}

#[derive(Debug, Deserialize)]
struct ListParams {
    owner: Option<String>,
    limit: Option<i64>,
}

/// `GET /api/knowledge` — newest first; `?owner=me` narrows to the caller's.
async fn list(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = params.limit.unwrap_or(100).clamp(1, 500) as usize;
    let owner = match params.owner.as_deref() {
        Some("me") => Some(caller.as_str().to_owned()),
        _ => None,
    };
    let notes = state
        .db
        .call(move |conn| store::list_knowledge(conn, owner.as_deref(), limit))
        .await?;
    Ok(Json(json!({ "notes": notes })))
}

/// `DELETE /api/knowledge/{id}` — author only.
async fn forget(
    State(state): State<AppState>,
    AuthUser(caller): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let owner = caller.as_str().to_owned();
    let outcome = state
        .db
        .call(move |conn| store::forget(conn, &id, &owner))
        .await?;
    match outcome {
        None => Err(ApiError::not_found("Knowledge note not found")),
        Some(false) => Err(ApiError::forbidden("Only the author can forget a note")),
        Some(true) => Ok(Json(json!({ "success": true }))),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use crate::routes::build;
    use crate::test_support::{register, send, state, wallet};

    /// Create a room as `token`, returning its id.
    async fn make_room(router: &axum::Router, token: &str, name: &str) -> String {
        let response = send(
            router,
            "POST",
            "/api/rooms",
            Some(token),
            Some(serde_json::json!({ "name": name })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        response.body["id"].as_str().unwrap().to_owned()
    }

    async fn post_message(router: &axum::Router, token: &str, room: &str, content: &str) {
        let hash = hex::encode(sha2::Sha256::digest(content.as_bytes()));
        let response = send(
            router,
            "POST",
            &format!("/api/rooms/{room}/messages"),
            Some(token),
            Some(serde_json::json!({ "content": content, "msgHash": hash })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    }

    use sha2::Digest;

    #[tokio::test]
    async fn a_posted_message_is_searchable_end_to_end() {
        let state = state("search-e2e");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let room = make_room(&router, &token, "General").await;
        post_message(
            &router,
            &token,
            &room,
            "the router password is #wifi swordfish",
        )
        .await;

        let response = send(
            &router,
            "GET",
            "/api/search?q=router%20password",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let results = response.body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["kind"], "message");
        assert_eq!(results[0]["roomId"], serde_json::json!(room));
        assert_eq!(results[0]["tags"], serde_json::json!(["wifi"]));
    }

    #[tokio::test]
    async fn search_needs_a_token() {
        let router = build(state("search-auth"));
        let response = send(&router, "GET", "/api/search?q=x", None, None).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn taught_knowledge_roundtrips_and_only_the_author_deletes() {
        let state = state("search-teach");
        let alice = wallet("alice");
        let bob = wallet("bob");
        let alice_token = register(&state, &alice, "alice");
        let bob_token = register(&state, &bob, "bob");
        let router = build(state);

        let response = send(
            &router,
            "POST",
            "/api/knowledge",
            Some(&alice_token),
            Some(serde_json::json!({ "content": "the spare key hangs by the door #home" })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        let note_id = response.body["id"].as_str().unwrap().to_owned();
        assert_eq!(response.body["tags"], serde_json::json!(["home"]));

        // Bob — no shared room — still finds it: knowledge is global.
        let response = send(
            &router,
            "GET",
            "/api/search?q=spare%20key",
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(response.body["results"].as_array().unwrap().len(), 1);

        // Bob cannot delete it.
        let response = send(
            &router,
            "DELETE",
            &format!("/api/knowledge/{note_id}"),
            Some(&bob_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);

        // Alice can.
        let response = send(
            &router,
            "DELETE",
            &format!("/api/knowledge/{note_id}"),
            Some(&alice_token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let response = send(
            &router,
            "GET",
            "/api/search?q=spare%20key",
            Some(&bob_token),
            None,
        )
        .await;
        assert!(response.body["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn membership_gates_message_results_over_http() {
        let state = state("search-scope");
        let alice = wallet("alice");
        let mallory = wallet("mallory");
        let alice_token = register(&state, &alice, "alice");
        let mallory_token = register(&state, &mallory, "mallory");
        let router = build(state);

        let room = make_room(&router, &alice_token, "Private").await;
        post_message(&router, &alice_token, &room, "the launch code is 0000").await;

        let response = send(
            &router,
            "GET",
            "/api/search?q=launch%20code",
            Some(&mallory_token),
            None,
        )
        .await;
        assert!(
            response.body["results"].as_array().unwrap().is_empty(),
            "a non-member must see nothing: {:?}",
            response.body
        );
    }

    #[tokio::test]
    async fn the_tag_cloud_counts_what_the_caller_sees() {
        let state = state("search-tags");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let room = make_room(&router, &token, "Notes").await;
        post_message(&router, &token, &room, "#recipe kimchi").await;
        post_message(&router, &token, &room, "#recipe bulgogi").await;
        post_message(&router, &token, &room, "#todo fix the fence").await;

        let response = send(&router, "GET", "/api/search/tags", Some(&token), None).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.body["tags"],
            serde_json::json!([
                { "tag": "recipe", "count": 2 },
                { "tag": "todo", "count": 1 },
            ])
        );
    }

    #[tokio::test]
    async fn bad_kind_and_oversized_query_are_rejected() {
        let state = state("search-validate");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let response = send(
            &router,
            "GET",
            "/api/search?q=x&kind=rooms",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        let long = "x".repeat(501);
        let response = send(
            &router,
            "GET",
            &format!("/api/search?q={long}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn teaching_nothing_is_rejected() {
        let state = state("search-teach-empty");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        for body in [
            serde_json::json!({}),
            serde_json::json!({ "content": "   " }),
        ] {
            let response = send(&router, "POST", "/api/knowledge", Some(&token), Some(body)).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn deleting_a_message_removes_it_from_search() {
        let state = state("search-delete");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");
        let router = build(state);

        let room = make_room(&router, &token, "General").await;
        post_message(&router, &token, &room, "ephemeral secret 12321").await;

        let response = send(
            &router,
            "GET",
            "/api/search?q=ephemeral",
            Some(&token),
            None,
        )
        .await;
        let results = response.body["results"].as_array().unwrap();
        let message_id = results[0]["refId"].as_str().unwrap().to_owned();

        let response = send(
            &router,
            "DELETE",
            &format!("/api/messages/{message_id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

        let response = send(
            &router,
            "GET",
            "/api/search?q=ephemeral",
            Some(&token),
            None,
        )
        .await;
        assert!(response.body["results"].as_array().unwrap().is_empty());
    }
}
