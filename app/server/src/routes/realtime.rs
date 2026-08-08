//! WebSocket and SSE transports (`docs/REALTIME.md` §8, §9).
//!
//! Both carry the same [`ServerEvent`] values with byte-identical JSON, and
//! neither ever carries message content. An event is a wake-up: "something
//! changed in room X, at serial N". The client then reads over REST, where
//! membership, blocking and E2EE are enforced once, in one place. That is why
//! losing an event costs latency and never data — and why the fan-out path
//! does not have to know anything about ciphertext.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use pocketskynet_core::{ClientMessage, RoomId, ServerEvent, Target, WalletAddress};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::auth::bearer_token;
use crate::db::now_ms;
use crate::error::{ApiError, ApiResult};
use crate::hub::{ConnHandle, ConnKind, HubError, ReplayError, MAX_REPLAY};
use crate::AppState;

/// Server ping cadence and the miss budget before a dead peer is dropped.
const PING_INTERVAL: Duration = Duration::from_secs(30);
const MAX_MISSED_PINGS: u32 = 2;

/// A socket with no traffic at all is closed. Any delivery resets it, so a
/// socket in an active room stays alive without pinging.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Server-enforced typing relay throttle, per connection.
const TYPING_MIN_INTERVAL_MS: i64 = 1000;

/// Largest frame accepted. Everything the socket takes is a tiny control
/// message; this is a memory guard, not a protocol limit.
const MAX_FRAME_BYTES: usize = 16 * 1024;

/// SSE streams are capped rather than run forever, which forces periodic
/// re-authorisation and a fresh membership/block snapshot.
const SSE_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// Heartbeat interval. Shorter than the WebSocket ping because SSE has no
/// protocol-level ping and must stay under the common 30–60 s proxy idle
/// window with margin.
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

const TICKET_TTL: Duration = Duration::from_secs(30);

// --------------------------------------------------------------- tickets ---

#[derive(Debug, Clone)]
struct Ticket {
    wallet: WalletAddress,
    expires_at_ms: i64,
    ip: IpAddr,
}

/// Short-lived, single-use credentials for `EventSource`, which cannot set an
/// `Authorization` header.
///
/// A ticket is 32 bytes of CSPRNG output, consumed atomically on use, valid
/// for 30 seconds, and bound to the wallet **and** the client address. A
/// ticket sitting in a proxy log is worthless within half a minute and
/// worthless immediately after use — unlike the multi-hour JWT it replaces.
pub struct TicketStore {
    tickets: DashMap<String, Ticket>,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketStore {
    pub fn new() -> Self {
        Self {
            tickets: DashMap::new(),
        }
    }

    /// Mint a ticket. Returns the opaque id and its expiry in epoch millis.
    ///
    /// Fallible only because the id is: a ticket is a bearer credential, and
    /// `random_hex_32` refuses rather than hand back a guessable one. The code
    /// below sweeps expired tickets before drawing the id, so on the rare
    /// entropy failure nothing new is inserted — though note that is a claim
    /// about the statements as written here, not one any server test exercises:
    /// the entropy seam is `#[cfg(test)]` inside `core` and unreachable from
    /// this crate, by design (see `core::random` on why there is no run-time
    /// substitution point).
    pub fn issue(&self, wallet: &WalletAddress, ip: IpAddr) -> ApiResult<(String, i64)> {
        let now = now_ms();
        // Sweeping on issue keeps the map bounded without a background task;
        // expired tickets are useless anyway.
        self.tickets.retain(|_, t| t.expires_at_ms > now);

        let id = format!("evt_{}", crate::auth::random_hex_32()?);
        let expires_at_ms = now + TICKET_TTL.as_millis() as i64;
        self.tickets.insert(
            id.clone(),
            Ticket {
                wallet: wallet.clone(),
                expires_at_ms,
                ip,
            },
        );
        Ok((id, expires_at_ms))
    }

    /// Redeem a ticket exactly once.
    ///
    /// Removal happens before validation, so even a ticket that fails the
    /// address or expiry check is spent — a stolen ticket cannot be retried
    /// from a different address until one attempt happens to work.
    pub fn consume(&self, id: &str, ip: IpAddr) -> Option<WalletAddress> {
        let (_, ticket) = self.tickets.remove(id)?;
        if ticket.expires_at_ms <= now_ms() {
            return None;
        }
        if ticket.ip != ip {
            return None;
        }
        Some(ticket.wallet)
    }

    pub fn len(&self) -> usize {
        self.tickets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tickets.is_empty()
    }
}

// --------------------------------------------------------------- routing ---

/// SSE endpoints live under `/api`, so they share CORS and rate limiting.
pub fn sse_router() -> Router<AppState> {
    Router::new()
        .route("/events", get(sse_handler))
        .route("/events/ticket", post(issue_ticket))
}

/// `/ws` sits outside `/api`, matching the reference's path exactly so
/// existing native clients need no change.
pub fn ws_router() -> Router<AppState> {
    Router::new().route("/ws", get(ws_handler))
}

/// The peer address, or loopback when the router was built without connection
/// info — which only happens in tests that drive the service directly.
///
/// A dedicated extractor rather than `ConnectInfo` itself, because
/// `ConnectInfo` *rejects* when it is absent and every caller here wants a
/// fallback instead of a 500.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddr(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(ip_of(parts)))
    }
}

/// Whether `wallet` is in the suspension set.
///
/// The spawned SSE task cannot call `AppState::is_suspended` — it owns only
/// what it captured — so the set travels as its own `Arc` and the comparison
/// lives here. No lowercasing on either side: [`WalletAddress`] normalises at
/// construction and the set is loaded lowercased, and re-lowercasing per event
/// would be paying for a case that cannot occur.
fn suspended(
    set: &std::sync::RwLock<std::collections::HashSet<String>>,
    wallet: &WalletAddress,
) -> bool {
    set.read()
        .map(|s| s.contains(wallet.as_str()))
        .unwrap_or(false)
}

fn ip_of(parts: &Parts) -> IpAddr {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Read one query parameter, percent-decoding its value.
///
/// Hand-rolled rather than pulling in a form decoder: the three parameters
/// this module reads are a JWT, an opaque ticket, and a room id, none of which
/// can contain a `&` or an `=`, and the decode is here only so a client that
/// escapes anyway still works.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                // Decode from the byte slice, not `raw[i+1..i+3]`: a `%` followed
                // by a multi-byte UTF-8 char would make that str-slice land inside
                // a codepoint and panic. `from_utf8` fails closed on non-ASCII here.
                let decoded = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match decoded {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `POST /api/events/ticket`.
async fn issue_ticket(
    State(state): State<AppState>,
    crate::auth::AuthUser(caller): crate::auth::AuthUser,
    ClientAddr(ip): ClientAddr,
) -> ApiResult<Response> {
    let (ticket, expires_at_ms) = state.tickets.issue(&caller, ip)?;

    Ok(Json(serde_json::json!({
        "ticket": ticket,
        "expiresAt": expires_at_ms / 1000,
        "ttlSeconds": TICKET_TTL.as_secs(),
    }))
    .into_response())
}

// ------------------------------------------------------------- extractor ---

/// A realtime credential, from whichever of the three transports carried it.
pub struct StreamAuth {
    pub wallet: WalletAddress,
    /// `exp` from the JWT, when the credential was one. A ticket-derived
    /// session has no expiry of its own and is bounded by the stream lifetime.
    pub token_exp: Option<i64>,
}

/// The token offered in `Sec-WebSocket-Protocol: fnauth, <jwt>`.
///
/// The header is split on commas and the first entry that is not the `fnauth`
/// marker is the credential. The server echoes only `fnauth`, never the token.
fn subprotocol_token(parts: &Parts) -> Option<String> {
    let raw = parts.headers.get("sec-websocket-protocol")?.to_str().ok()?;
    raw.split(',')
        .map(str::trim)
        .find(|entry| !entry.is_empty() && *entry != "fnauth")
        .map(str::to_owned)
}

impl StreamAuth {
    /// Resolve a credential from whichever transport carried it.
    ///
    /// `allow_query_token` is a parameter rather than a constant because the
    /// two specs gate it differently and the decision is worth keeping in one
    /// visible place: `REALTIME.md` §8.1 puts the **SSE** query token behind a
    /// config flag, default off, while `API.md` §12.1 presents the
    /// **WebSocket** one as an unconditional fallback for clients that cannot
    /// set `Sec-WebSocket-Protocol`.
    ///
    /// Both transports honour the flag here. A full-lifetime bearer token in a
    /// URL lands in access logs, `Referer` headers and browser history
    /// whichever protocol carried it, and both transports have a header-based
    /// path that works — the subprotocol for WebSocket, a ticket for SSE. An
    /// operator who needs the fallback for a proxy that strips the subprotocol
    /// turns on `--sse-token-query`.
    fn resolve(parts: &Parts, state: &AppState, allow_query_token: bool) -> ApiResult<Self> {
        let query = parts.uri.query().map(str::to_owned);
        let ticket = query_param(query.as_deref(), "ticket");
        let query_token = query_param(query.as_deref(), "token");
        let ip = ip_of(parts);

        // 1. Ticket — the recommended path for EventSource.
        if let Some(id) = ticket.as_deref() {
            let wallet = state
                .tickets
                .consume(id, ip)
                .ok_or_else(|| ApiError::unauthorized("Invalid token"))?;
            return Ok(Self {
                wallet,
                token_exp: None,
            });
        }

        // 2. A real Authorization header, or the WebSocket subprotocol.
        let header_token = bearer_token(&parts.headers)
            .map(str::to_owned)
            .or_else(|| subprotocol_token(parts));
        if let Some(token) = header_token {
            let (wallet, claims) = state.jwt.verify(&token)?;
            return Ok(Self {
                wallet,
                token_exp: Some(claims.exp),
            });
        }

        // 3. `?token=`. A full-lifetime bearer token in a URL lands in access
        // logs, `Referer` headers and browser history, and a long-lived stream
        // is re-established often enough to multiply the exposure — hence the
        // warning on every use, and the flag on the transport that has an
        // alternative.
        if let Some(token) = query_token.as_deref() {
            if !allow_query_token {
                // Say *why*. "Invalid token" sends the reader off to check a
                // credential that is very likely fine, when the actual problem
                // is a server flag they cannot see from the client side.
                return Err(ApiError::unauthorized(
                    "Query-string tokens are disabled on this server; \
                     use the Sec-WebSocket-Protocol handshake or an SSE ticket, \
                     or start the server with --sse-token-query",
                ));
            }
            tracing::warn!("realtime credential supplied in a query string");
            let (wallet, claims) = state.jwt.verify(token)?;
            return Ok(Self {
                wallet,
                token_exp: Some(claims.exp),
            });
        }

        Err(ApiError::unauthorized("No token provided"))
    }

    /// [`resolve`](Self::resolve), then the suspension gate.
    ///
    /// This wrapper exists because `resolve` has four success exits and a
    /// check pasted after each is the kind that loses one in a refactor. The
    /// audit that prompted it found exactly that shape of bug already shipped:
    /// `AuthUser` refused suspended accounts on every REST request, but these
    /// stream credentials verified the JWT directly — so a suspended user who
    /// ignored the advisory `SessionExpired` event could simply reconnect
    /// over WebSocket or SSE and keep receiving room activity, typing signals
    /// and serials until their token expired. Events are wake-up signals, not
    /// content, but "suspension takes effect immediately" has to mean the
    /// streams too — including a ticket minted moments before the suspension
    /// landed, which is why the check sits after ticket consumption rather
    /// than only on the JWT paths.
    fn resolve_active(parts: &Parts, state: &AppState, allow_query_token: bool) -> ApiResult<Self> {
        let auth = Self::resolve(parts, state, allow_query_token)?;
        if state.is_suspended(auth.wallet.as_str()) {
            return Err(ApiError::unauthorized(
                "This account has been suspended by a server administrator.",
            ));
        }
        Ok(auth)
    }
}

/// SSE credential: ticket, bearer header, or — behind `--sse-token-query` —
/// `?token=`.
pub struct SseAuth(pub StreamAuth);

impl FromRequestParts<AppState> for SseAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        StreamAuth::resolve_active(parts, state, state.cfg.sse_token_query).map(Self)
    }
}

/// WebSocket credential: subprotocol (preferred), bearer header, ticket, or —
/// behind `--sse-token-query` — `?token=`. See [`StreamAuth::resolve`] for why
/// the flag covers this transport too.
pub struct WsAuth(pub StreamAuth);

impl FromRequestParts<AppState> for WsAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        StreamAuth::resolve_active(parts, state, state.cfg.sse_token_query).map(Self)
    }
}

// ------------------------------------------------------------------- SSE ---

/// `GET /api/events` — resumable multiplexed event stream.
async fn sse_handler(
    State(state): State<AppState>,
    SseAuth(auth): SseAuth,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // A malformed `Last-Event-ID` is treated as "no cursor" — live tail only —
    // rather than as an error: the client cannot fix it, and live tail is a
    // strictly safe degradation.
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let single_room = query_param(uri.query(), "room");

    if !state.hub.has_capacity() {
        return Err(ApiError::TooManyRequests("Server at capacity".into()));
    }

    let mut view =
        state.hub.load_view(&auth.wallet).await.map_err(|e| {
            ApiError::Internal(anyhow::Error::new(e).context("loading stream view"))
        })?;

    // A single-room stream is a narrowing of the same view, never a widening:
    // asking for a room you are not in yields an empty subscription set rather
    // than access.
    if let Some(room) = single_room.as_deref() {
        let requested = RoomId::new(room).map_err(|_| ApiError::bad_request("Invalid room id"))?;
        view.rooms = view
            .rooms
            .intersection(&HashSet::from([requested]))
            .cloned()
            .collect();
    }

    let handle = state
        .hub
        .register(ConnHandle::new(
            state.hub.next_conn_id(),
            auth.wallet.clone(),
            ConnKind::Sse,
            view,
            auth.token_exp,
        ))
        .map_err(|e| match e {
            HubError::AtCapacity | HubError::TooManyConnections => {
                ApiError::TooManyRequests("Too many connections".into())
            }
            other => ApiError::Internal(anyhow::Error::new(other)),
        })?;

    let (tx, rx) = mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let mut receiver = state.hub.subscribe();

    // Registration, then subscription, then the announcement. See the same
    // sequence in `ws_conn`: announcing awaits a database read, so doing it
    // before this receiver exists would silently drop every event published
    // during the wait.
    state.hub.announce_presence(&auth.wallet).await;
    let hub = state.hub.clone();
    let suspensions = state.suspensions.clone();
    let conn_id = handle.id;

    tokio::spawn(async move {
        // Tell the browser how long to wait before reconnecting; without it
        // the default is 3 s anyway, but stating it keeps the two ends
        // agreeing after a deploy.
        if tx
            .send(Ok(Event::default().retry(Duration::from_secs(3))))
            .await
            .is_err()
        {
            hub.disconnect(conn_id).await;
            return;
        }

        // Replay before switching to live tail, so the gap is closed in
        // order and the cursor never goes backwards.
        if let Some(cursor) = cursor {
            let view = handle.view();
            match hub.replay_since(cursor, &view, &handle.wallet, MAX_REPLAY) {
                Ok(envelopes) => {
                    for envelope in envelopes {
                        if tx
                            .send(Ok(sse_event(envelope.seq, &envelope.event)))
                            .await
                            .is_err()
                        {
                            hub.disconnect(conn_id).await;
                            return;
                        }
                    }
                }
                Err(ReplayError::CursorTooOld) | Err(ReplayError::Log(_)) => {
                    // Bounded replay, unbounded correctness: the client does a
                    // full sync per room rather than receiving a silent gap.
                    let event = ServerEvent::ResyncRequired {
                        reason: pocketskynet_core::ResyncReason::CursorTooOld,
                        from_seq: cursor,
                        to_seq: hub.lagged_seq(),
                    };
                    let _ = tx.send(Ok(sse_event(hub.lagged_seq(), &event))).await;
                }
            }
        }

        let deadline = tokio::time::Instant::now() + SSE_MAX_LIFETIME;

        loop {
            tokio::select! {
                _ = handle.cancel.cancelled() => break,
                // The client hung up. Dropping the response body drops the
                // stream this channel feeds, and `closed()` is the only thing
                // that reports it *without* something to send: every other exit
                // here is a send that fails, and an idle stream never sends.
                //
                // Without this, a browser closing a tab left a registered
                // connection behind until the 30-minute lifetime cap or the
                // next event addressed to it — whichever came first. It cost a
                // slot against the 5000-connection and 8-per-wallet caps, and
                // once presence started reading the connection index it became
                // visible: the person who shut their laptop stayed lit for half
                // an hour, which is the exact opposite of what presence is for.
                _ = tx.closed() => break,
                _ = tokio::time::sleep_until(deadline) => {
                    let event = ServerEvent::SessionExpired { reason: "stream_lifetime".into() };
                    let _ = tx.send(Ok(sse_event(hub.lagged_seq(), &event))).await;
                    break;
                }
                received = receiver.recv() => match received {
                    Ok(envelope) => {
                        let view = handle.view();
                        if !view.accepts(&envelope, &handle.wallet) {
                            continue;
                        }
                        if handle.token_expired(now_ms() / 1000) {
                            let event = ServerEvent::SessionExpired { reason: "token_expired".into() };
                            let _ = tx.send(Ok(sse_event(envelope.seq, &event))).await;
                            break;
                        }
                        // Checked at the same spot as expiry, and for the same
                        // reason: a stream opened before the suspension landed
                        // is a credential the deny set cannot reach any other
                        // way. Gating *delivery* is the guarantee that
                        // matters — no event crosses after the suspension,
                        // however long the socket itself lingers.
                        if suspended(&suspensions, &handle.wallet) {
                            let event = ServerEvent::SessionExpired { reason: "suspended".into() };
                            let _ = tx.send(Ok(sse_event(envelope.seq, &event))).await;
                            break;
                        }
                        if tx.send(Ok(sse_event(envelope.seq, &envelope.event))).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Not an error to hide: the client is told to resync,
                        // which is always correct for wake-up events.
                        let event = hub.lagged_event(0);
                        if tx.send(Ok(sse_event(hub.lagged_seq(), &event))).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }

        hub.disconnect(conn_id).await;
    });

    let stream = ReceiverStream::new(rx);
    let sse = Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_HEARTBEAT).text("hb"));

    let mut response = sse.into_response();
    // Defeat nginx's proxy buffering, which would otherwise hold frames until
    // a buffer filled and make the stream look dead.
    response.headers_mut().insert(
        "x-accel-buffering",
        axum::http::HeaderValue::from_static("no"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

/// Encode one event as an SSE frame.
///
/// Transient events carry no `id:`, so a `Last-Event-ID` resume never replays
/// a stale typing indicator and the client's cursor cannot regress.
fn sse_event(seq: u64, event: &ServerEvent) -> Event {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_owned());
    let frame = Event::default().event(event.name()).data(data);
    if event.is_replayable() {
        frame.id(seq.to_string())
    } else {
        frame
    }
}

// ------------------------------------------------------------- WebSocket ---

/// `GET /ws`.
async fn ws_handler(
    State(state): State<AppState>,
    WsAuth(auth): WsAuth,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        // Echo the marker only — the token must never come back out.
        .protocols(["fnauth"])
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| ws_conn(socket, state, auth))
}

/// Close codes, reused verbatim from the reference so existing native clients
/// need no changes.
mod close {
    pub const AUTH: u16 = 4001;
    pub const IDLE: u16 = 4008;
    pub const CAPACITY: u16 = 4013;
    pub const SERVER: u16 = 4500;
}

async fn ws_conn(socket: WebSocket, state: AppState, auth: StreamAuth) {
    let (mut sink, mut stream) = socket.split();

    // Capacity is checked before any per-connection work, so a flood cannot
    // make the server load views it will immediately discard.
    if !state.hub.has_capacity() {
        let _ = sink
            .send(close_frame(close::CAPACITY, "Server at capacity"))
            .await;
        return;
    }

    let view = match state.hub.load_view(&auth.wallet).await {
        Ok(view) => view,
        Err(e) => {
            tracing::warn!(error = %e, "could not load the connection view");
            let _ = sink
                .send(close_frame(close::SERVER, "Failed to load rooms"))
                .await;
            return;
        }
    };

    let handle = match state.hub.register(ConnHandle::new(
        state.hub.next_conn_id(),
        auth.wallet.clone(),
        ConnKind::Ws,
        view,
        auth.token_exp,
    )) {
        Ok(handle) => handle,
        Err(HubError::TooManyConnections) => {
            let _ = sink
                .send(close_frame(close::CAPACITY, "Too many connections"))
                .await;
            return;
        }
        Err(HubError::AtCapacity) => {
            let _ = sink
                .send(close_frame(close::CAPACITY, "Server at capacity"))
                .await;
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not register the connection");
            let _ = sink
                .send(close_frame(close::SERVER, "Failed to load rooms"))
                .await;
            return;
        }
    };

    let mut receiver = state.hub.subscribe();

    // Registration, then subscription, then the announcement — in that order,
    // and the order is load-bearing.
    //
    // *After* registration because the answer is derived from the connection
    // index this socket has only just been added to. *After* `subscribe`
    // because announcing awaits a database read, and until this receiver
    // exists every event published by anybody else is one this connection
    // never sees. There was no gap to fall into before, only because nothing
    // between the two ever yielded; adding the first `await` there opened one,
    // and it presented as a typing indicator that occasionally vanished on a
    // loaded machine.
    state.hub.announce_presence(&handle.wallet).await;

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick is immediate; skip it
    let mut missed_pings = 0u32;
    let mut idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;

    let closing = loop {
        tokio::select! {
            _ = handle.cancel.cancelled() => break None,

            _ = tokio::time::sleep_until(idle_deadline) => {
                break Some((close::IDLE, "Idle timeout"));
            }

            _ = ping.tick() => {
                if handle.token_expired(now_ms() / 1000) {
                    break Some((close::AUTH, "Token expired"));
                }
                if state.is_suspended(handle.wallet.as_str()) {
                    break Some((close::AUTH, "Account suspended"));
                }
                missed_pings += 1;
                if missed_pings > MAX_MISSED_PINGS {
                    // No close frame: the peer is not answering, so there is
                    // nobody to tell.
                    break None;
                }
                if sink.send(Message::Ping(axum::body::Bytes::new())).await.is_err() {
                    break None;
                }
            }

            received = receiver.recv() => match received {
                Ok(envelope) => {
                    let view = handle.view();
                    if !view.accepts(&envelope, &handle.wallet) {
                        continue;
                    }
                    // The ping tick above is a 30-second cadence; this is the
                    // moment an event would actually cross, so it is the one
                    // that must not be later than the suspension.
                    if state.is_suspended(handle.wallet.as_str()) {
                        break Some((close::AUTH, "Account suspended"));
                    }
                    let payload = serde_json::to_string(&*envelope.event).unwrap_or_default();
                    if sink.send(Message::Text(payload.into())).await.is_err() {
                        break None;
                    }
                    // Delivery counts as activity: a socket in a busy room
                    // stays alive without the client saying anything.
                    idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let event = state.hub.lagged_event(0);
                    let payload = serde_json::to_string(&event).unwrap_or_default();
                    if sink.send(Message::Text(payload.into())).await.is_err() {
                        break None;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break None,
            },

            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                    missed_pings = 0;
                    // Unparseable frames and unknown types are ignored rather
                    // than answered: an error reply would hand an unauthorised
                    // shape a free amplification primitive, and the protocol
                    // has to stay forward-compatible anyway.
                    match serde_json::from_str::<ClientMessage>(text.as_str()) {
                        Ok(ClientMessage::Ping) => {
                            state.hub.note_activity(&handle).await;
                            let pong =
                                serde_json::to_string(&ServerEvent::Pong).unwrap_or_default();
                            if sink.send(Message::Text(pong.into())).await.is_err() {
                                break None;
                            }
                        }
                        Ok(ClientMessage::Typing { room_id }) => {
                            state.hub.note_activity(&handle).await;
                            relay_typing(&state, &handle, room_id).await;
                        }
                        Ok(ClientMessage::Presence { status }) => {
                            // A client may say it stepped away or came back;
                            // it may not claim to be offline over a socket it
                            // is holding open. Refused silently, like every
                            // other frame this endpoint declines.
                            if status.is_declarable() {
                                state.hub.declare_presence(&handle, status).await;
                            }
                        }
                        Err(_) => {}
                    }
                }
                // Answering a protocol ping keeps the socket alive but is
                // deliberately *not* presence activity: the browser replies
                // from its own network stack whether or not the page's
                // JavaScript is running, so counting it would make the idle
                // threshold unreachable for every open tab. The client's own
                // `ping` frame above is the one that stops arriving when a tab
                // is frozen, and that is the signal presence wants.
                Some(Ok(Message::Pong(_))) => {
                    missed_pings = 0;
                }
                Some(Ok(Message::Close(_))) | None => break None,
                Some(Ok(_)) => {}
                Some(Err(_)) => break None,
            },
        }
    };

    if let Some((code, reason)) = closing {
        let _ = sink.send(close_frame(code, reason)).await;
    }
    state.hub.disconnect(handle.id).await;
}

fn close_frame(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.to_owned().into(),
    }))
}

/// Relay a typing indicator, subject to two silent gates.
///
/// Membership comes from the server-derived subscription set, so a client
/// cannot relay into a room it is not in, and the 1/s throttle is enforced
/// server-side regardless of what the client self-imposes. Both gates drop the
/// frame without a reply: an error would be noisier than the signal it refuses.
///
/// The target excludes the sender, and the envelope carries `origin`, so the
/// hub's block filter keeps the indicator from crossing a block in either
/// direction. Typing is a presence side-channel — unfiltered it would reveal
/// that a blocked user is active in a shared room.
async fn relay_typing(state: &AppState, handle: &ConnHandle, room_id: RoomId) {
    let view = handle.view();
    if !view.rooms.contains(&room_id) {
        return;
    }
    if !handle.allow_typing(now_ms(), TYPING_MIN_INTERVAL_MS) {
        return;
    }
    state
        .hub
        .publish_best_effort(
            Target::RoomExcept {
                room_id: room_id.clone(),
                except: handle.wallet.clone(),
            },
            Some(handle.wallet.clone()),
            ServerEvent::Typing {
                room_id,
                from: handle.wallet.clone(),
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build;
    use crate::test_support::{register, send, send_head, state, wallet};
    use axum::http::StatusCode;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn a_ticket_can_only_be_redeemed_once() {
        let store = TicketStore::new();
        let alice = wallet("alice");
        let (ticket, _) = store.issue(&alice, ip(1)).unwrap();

        assert_eq!(store.consume(&ticket, ip(1)), Some(alice));
        assert_eq!(
            store.consume(&ticket, ip(1)),
            None,
            "a leaked ticket must be worthless after use"
        );
    }

    #[test]
    fn a_ticket_is_bound_to_the_address_that_asked_for_it() {
        let store = TicketStore::new();
        let alice = wallet("alice");
        let (ticket, _) = store.issue(&alice, ip(1)).unwrap();

        assert_eq!(store.consume(&ticket, ip(2)), None);
        // The attempt spent it, so the rightful owner is refused too — a
        // stolen ticket cannot be brute-forced across addresses.
        assert_eq!(store.consume(&ticket, ip(1)), None);
    }

    #[test]
    fn an_unknown_ticket_is_simply_absent() {
        let store = TicketStore::new();
        assert_eq!(store.consume("evt_nope", ip(1)), None);
        assert!(store.is_empty());
    }

    #[test]
    fn percent_decode_handles_valid_and_malformed_escapes() {
        assert_eq!(percent_decode("hello"), "hello");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        // A trailing/short escape is left literal, not decoded.
        assert_eq!(percent_decode("z%2"), "z%2");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn percent_decode_does_not_panic_on_multibyte_after_percent() {
        // Regression: `%` followed by a multi-byte UTF-8 char used to slice the
        // str inside a codepoint and panic — reachable unauthenticated via /ws.
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%é"), "%é");
        assert_eq!(percent_decode("x%🔥y"), "x%🔥y");
    }

    #[test]
    fn issuing_sweeps_expired_tickets() {
        let store = TicketStore::new();
        let alice = wallet("alice");
        store.tickets.insert(
            "evt_stale".into(),
            Ticket {
                wallet: alice.clone(),
                expires_at_ms: now_ms() - 1,
                ip: ip(1),
            },
        );

        store.issue(&alice, ip(1)).unwrap();
        assert_eq!(store.len(), 1, "the stale entry must not accumulate");
        assert!(store.consume("evt_stale", ip(1)).is_none());
    }

    #[tokio::test]
    async fn a_ticket_needs_a_valid_token_to_mint() {
        let state = state("ticket-auth");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let anonymous = send(&router, "POST", "/api/events/ticket", None, None).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

        let issued = send(&router, "POST", "/api/events/ticket", Some(&token), None).await;
        assert_eq!(issued.status, StatusCode::OK);
        assert!(issued.json()["ticket"]
            .as_str()
            .unwrap()
            .starts_with("evt_"));
        assert_eq!(issued.json()["ttlSeconds"], 30);
    }

    #[tokio::test]
    async fn the_event_stream_refuses_an_anonymous_caller() {
        let router = build(state("sse-auth"));
        let response = send(&router, "GET", "/api/events", None, None).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
        assert_eq!(response.json()["message"], "No token provided");
    }

    #[tokio::test]
    async fn an_open_stream_announces_itself_as_unbuffered_event_stream() {
        let state = state("sse-headers");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let issued = send(&router, "POST", "/api/events/ticket", Some(&token), None).await;
        let ticket = issued.json()["ticket"].as_str().unwrap().to_owned();

        let (status, headers) = send_head(
            &router,
            "GET",
            &format!("/api/events?ticket={ticket}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        assert_eq!(headers["cache-control"], "no-store");
        // Without this, nginx buffers the stream and it looks dead.
        assert_eq!(headers["x-accel-buffering"], "no");
    }

    #[tokio::test]
    async fn the_event_stream_refuses_a_spent_ticket() {
        let state = state("sse-spent");
        let token = register(&state, &wallet("alice"), "alice");
        let router = build(state);

        let issued = send(&router, "POST", "/api/events/ticket", Some(&token), None).await;
        let ticket = issued.json()["ticket"].as_str().unwrap().to_owned();

        // Two uses of one ticket: the second must fail.
        let (first, _) = send_head(
            &router,
            "GET",
            &format!("/api/events?ticket={ticket}"),
            None,
        )
        .await;
        assert_eq!(first, StatusCode::OK);

        let second = send(
            &router,
            "GET",
            &format!("/api/events?ticket={ticket}"),
            None,
            None,
        )
        .await;
        assert_eq!(second.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn the_subprotocol_token_is_whatever_is_not_the_marker() {
        let build_parts = |value: &str| {
            let request = axum::http::Request::builder()
                .uri("/ws")
                .header("sec-websocket-protocol", value)
                .body(())
                .unwrap();
            request.into_parts().0
        };

        assert_eq!(
            subprotocol_token(&build_parts("fnauth, abc.def.ghi")).as_deref(),
            Some("abc.def.ghi")
        );
        assert_eq!(
            subprotocol_token(&build_parts("abc.def.ghi,fnauth")).as_deref(),
            Some("abc.def.ghi")
        );
        assert_eq!(subprotocol_token(&build_parts("fnauth")), None);
    }

    #[test]
    fn transient_events_carry_no_resume_id() {
        let room = RoomId::new("room_1749652739650_ab").unwrap();
        let typing = ServerEvent::Typing {
            room_id: room.clone(),
            from: wallet("alice"),
        };
        let durable = ServerEvent::NewMessage {
            room_id: room,
            msg_serial: 5,
        };

        // The public API of `Event` hides its fields, so the assertion is on
        // the property that drives the branch.
        assert!(!typing.is_replayable());
        assert!(durable.is_replayable());
        let _ = sse_event(1, &typing);
        let _ = sse_event(2, &durable);
    }

    #[tokio::test]
    async fn the_query_token_fallback_honours_its_config_flag() {
        let mut state = state("sse-token-query");
        let alice = wallet("alice");
        let token = register(&state, &alice, "alice");

        // Enabled in the test fixture: the stream opens.
        let router = build(state.clone());
        let (allowed, _) =
            send_head(&router, "GET", &format!("/api/events?token={token}"), None).await;
        assert_eq!(allowed, StatusCode::OK);

        // Disabled: refused, because a bearer token in a URL ends up in logs.
        let mut cfg = (*state.cfg).clone();
        cfg.sse_token_query = false;
        state.cfg = std::sync::Arc::new(cfg);
        let router = build(state);

        let refused = send(
            &router,
            "GET",
            &format!("/api/events?token={token}"),
            None,
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    }
}
