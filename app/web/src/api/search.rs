//! Search & knowledge endpoints (docs/SEARCH.md §4).
//!
//! Retrieval is the server's whole contribution: these calls return ranked
//! passages and taught notes. Turning passages into an AI answer is the
//! Knowledge page's business, with the user's consent, via `crate::ai`.

use gloo_net::http::Method;
use serde::{Deserialize, Serialize};

use super::{encode_query, encode_segment, ApiResult, Client};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SearchHit {
    /// `"message"` or `"knowledge"`.
    pub kind: String,
    #[serde(rename = "refId")]
    pub ref_id: String,
    #[serde(rename = "roomId")]
    pub room_id: Option<String>,
    pub sender: Option<String>,
    pub timestamp: i64,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Orders results within one response only.
    #[serde(default)]
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KnowledgeNote {
    pub id: String,
    #[serde(rename = "ownerAddress")]
    pub owner_address: String,
    pub content: String,
    #[serde(rename = "roomId")]
    pub room_id: Option<String>,
    #[serde(rename = "sourceMessageId")]
    pub source_message_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchHit>,
}

#[derive(Deserialize)]
struct TagsResponse {
    tags: Vec<TagCount>,
}

#[derive(Deserialize)]
struct NotesResponse {
    notes: Vec<KnowledgeNote>,
}

#[derive(Serialize)]
struct TeachRequest<'a> {
    content: &'a str,
    #[serde(rename = "roomId", skip_serializing_if = "Option::is_none")]
    room_id: Option<&'a str>,
    #[serde(rename = "sourceMessageId", skip_serializing_if = "Option::is_none")]
    source_message_id: Option<&'a str>,
}

impl Client {
    /// `GET /api/search` — hybrid retrieval over everything visible. An empty
    /// or tag-only query browses newest-first.
    pub async fn search(&self, query: &str, limit: usize) -> ApiResult<Vec<SearchHit>> {
        let response: SearchResponse = self
            .send(
                Method::GET,
                &format!("/api/search?q={}&limit={limit}", encode_query(query)),
            )
            .await?;
        Ok(response.results)
    }

    /// `GET /api/search/tags` — the visible tag cloud, most-used first.
    pub async fn search_tags(&self, limit: usize) -> ApiResult<Vec<TagCount>> {
        let response: TagsResponse = self
            .send(Method::GET, &format!("/api/search/tags?limit={limit}"))
            .await?;
        Ok(response.tags)
    }

    /// `POST /api/knowledge` — teach a note.
    pub async fn teach(
        &self,
        content: &str,
        room_id: Option<&str>,
        source_message_id: Option<&str>,
    ) -> ApiResult<KnowledgeNote> {
        self.send_json(
            Method::POST,
            "/api/knowledge",
            &TeachRequest {
                content,
                room_id,
                source_message_id,
            },
        )
        .await
    }

    /// `GET /api/knowledge` — taught notes, newest first.
    pub async fn knowledge(&self, limit: usize) -> ApiResult<Vec<KnowledgeNote>> {
        let response: NotesResponse = self
            .send(Method::GET, &format!("/api/knowledge?limit={limit}"))
            .await?;
        Ok(response.notes)
    }

    /// `DELETE /api/knowledge/{id}` — forget a note (author only; 403 otherwise).
    pub async fn forget(&self, id: &str) -> ApiResult<()> {
        self.send_ok_empty(
            Method::DELETE,
            &format!("/api/knowledge/{}", encode_segment(id)),
        )
        .await
    }
}
