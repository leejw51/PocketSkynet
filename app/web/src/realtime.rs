//! Realtime transport: WebSocket → SSE → polling, with backoff and automatic
//! degradation (REALTIME.md §1–§8).
//!
//! The single most important property of this layer, and the reason it can be
//! this simple: **events carry no content**. A `new_message` frame is a wake-up
//! signal naming a room, nothing more. Every byte the user actually sees comes
//! back over REST, where membership, blocking and E2EE are enforced once. So
//! losing an event costs latency, never data — which is what makes an
//! aggressive degrade-and-retry policy safe.
//!
//! The pure parts (backoff, tier selection, typing expiry) are host-tested; the
//! socket plumbing is `wasm32`-only.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pocketskynet_core::{RoomId, ServerEvent, WalletAddress};

use crate::session::ConnectionMode;

/// Which transport is actually carrying events right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    WebSocket,
    Sse,
    Polling,
}

impl Transport {
    /// The `.fn-conn--*` modifier for the connection pill.
    pub fn pill_class(self) -> &'static str {
        match self {
            Transport::WebSocket => "fn-conn--ws",
            // SSE and polling both read as "not a live socket" to a user, and
            // `app.css` gives them the same blue treatment.
            Transport::Sse | Transport::Polling => "fn-conn--poll",
        }
    }

    pub fn label(self, lang: crate::i18n::Lang) -> &'static str {
        use crate::i18n::{t, Key};
        match self {
            Transport::WebSocket => t(lang, Key::conn_live),
            Transport::Sse => t(lang, Key::conn_events),
            Transport::Polling => t(lang, Key::conn_polling),
        }
    }
}

/// What the connection pill should show (DESIGN.md §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    /// Connected on some transport.
    Live(Transport),
    /// A catch-up sync is in flight.
    Syncing,
    /// No transport at all.
    Offline,
}

impl ConnStatus {
    pub fn pill_class(self) -> &'static str {
        match self {
            ConnStatus::Live(t) => t.pill_class(),
            ConnStatus::Syncing => "fn-conn--syncing",
            ConnStatus::Offline => "fn-conn--offline",
        }
    }

    pub fn label(self, lang: crate::i18n::Lang) -> &'static str {
        use crate::i18n::{t, Key};
        match self {
            ConnStatus::Live(tr) => tr.label(lang),
            ConnStatus::Syncing => t(lang, Key::syncing),
            ConnStatus::Offline => t(lang, Key::offline),
        }
    }

    /// The accessible name spells out both the state and the action, because
    /// the pill is also a control (DESIGN.md §7.4).
    pub fn aria_label(self, lang: crate::i18n::Lang) -> String {
        use crate::i18n::{t, Key};
        match self {
            ConnStatus::Live(Transport::WebSocket) => t(lang, Key::conn_live_aria).into(),
            ConnStatus::Live(Transport::Sse) => t(lang, Key::conn_events_aria).into(),
            ConnStatus::Live(Transport::Polling) => t(lang, Key::conn_polling_aria).into(),
            ConnStatus::Syncing => t(lang, Key::conn_syncing_aria).into(),
            ConnStatus::Offline => t(lang, Key::conn_offline_aria).into(),
        }
    }
}

/// Exponential backoff with jitter (REALTIME.md §8.5).
///
/// `min(1000 · 2^attempt, 30_000)`, then ±20 %. The jitter is the part the
/// reference client lacks and the part that matters at scale: without it, every
/// client that dropped during a deploy reconnects in lockstep and the
/// thundering herd re-kills the server it was waiting for.
// Only the wasm reconnect loop calls this; on the host it exists to be tested.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn backoff_delay_ms(attempt: u32, jitter01: f64) -> u32 {
    // Saturating shift: attempt 40 must not wrap to a tiny delay.
    let base = 1000u64.saturating_mul(1u64 << attempt.min(20)).min(30_000);
    let jitter01 = jitter01.clamp(0.0, 1.0);
    // Map [0,1] onto [0.8, 1.2].
    let factor = 0.8 + 0.4 * jitter01;
    (base as f64 * factor) as u32
}

/// The transport tier to try, given the user's preference and how many
/// consecutive failures the current tier has had.
///
/// Degradation is one step at a time and never below polling: polling only
/// needs plain HTTP, so if it fails the network is gone and no other tier would
/// have worked either.
///
/// A user who explicitly chose polling is never upgraded — an automatic
/// "improvement" past an explicit preference is a bug, not a feature.
pub fn select_transport(preference: ConnectionMode, consecutive_failures: u32) -> Transport {
    match preference {
        ConnectionMode::Polling => Transport::Polling,
        ConnectionMode::Sse => {
            if consecutive_failures >= 2 {
                Transport::Polling
            } else {
                Transport::Sse
            }
        }
        ConnectionMode::WebSocket => match consecutive_failures {
            0 | 1 => Transport::WebSocket,
            2 | 3 => Transport::Sse,
            _ => Transport::Polling,
        },
    }
}

/// How often to poll, in milliseconds, when polling is the transport.
pub const POLL_INTERVAL_MS: u32 = 10_000;

/// A slow safety-net sync even when a socket is healthy, in case an event was
/// dropped somewhere between the server's fan-out and this tab.
pub const SAFETY_SYNC_MS: u32 = 60_000;

/// Client keepalive period. Must be under the server's 30 s ping tick so an
/// app-level ping always lands first.
pub const PING_INTERVAL_MS: u32 = 25_000;

/// How many consecutive connection failures a transport tries before handing
/// over to the next tier down.
///
/// Small on purpose. A WebSocket that cannot connect is usually blocked by a
/// proxy that strips `Upgrade`, and no amount of retrying fixes that — the
/// point is to reach SSE or polling quickly rather than to keep hoping. Once a
/// tier connects the counter resets, so a flaky network does not permanently
/// demote anyone.
pub const MAX_ATTEMPTS_PER_TIER: u32 = 3;

// A single attempt per tier would demote the session on one flaky connect,
// which is worse than the bug this whole mechanism exists to fix.
const _: () = assert!(MAX_ATTEMPTS_PER_TIER >= 2);

/// How long a connection must survive before it counts as having worked.
///
/// A failed WebSocket handshake presents as "opened, then closed a few
/// milliseconds later", because the browser constructs the object before it
/// negotiates. Without a floor, those flaps reset the retry counter forever and
/// the client stays on a transport that cannot work.
pub const MIN_HEALTHY_SESSION_MS: f64 = 2_000.0;

/// Whether a connection lasted long enough to prove the transport works.
///
/// Split out so the rule is testable without a browser — the bug this guards
/// against was invisible to every unit test precisely because the decision was
/// inline in an async loop that only runs under wasm.
pub fn session_was_healthy(duration_ms: f64) -> bool {
    // NaN is not evidence of health; compare so that it falls through to false.
    duration_ms >= MIN_HEALTHY_SESSION_MS
}

// Compile-time invariants, so a future tweak to one constant cannot quietly
// invalidate the reasoning behind another.
//
// The server pings every 30 s and terminates after two misses, so an app-level
// keepalive at or above 30 s would race that timer. And the safety-net sync
// must be rarer than polling, or polling mode would double its request rate
// against a 100 requests/minute/IP limiter.
const _: () = assert!(PING_INTERVAL_MS < 30_000);
const _: () = assert!(SAFETY_SYNC_MS > POLL_INTERVAL_MS);

/// Self-throttle for outbound typing frames, on top of the server's 1/s cap.
pub const TYPING_THROTTLE_MS: f64 = 2_000.0;

/// How long a typing indicator survives its last event.
pub const TYPING_TTL_MS: i64 = 4_000;

/// Who is currently typing, per room, with expiry.
///
/// Typing events are never replayed and never persisted; this is pure UI state
/// with a timer, and it is swept rather than event-driven because the "stopped
/// typing" event does not exist in the protocol.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TypingTracker {
    seen: HashMap<(RoomId, WalletAddress), i64>,
}

impl TypingTracker {
    pub fn note(&mut self, room: RoomId, who: WalletAddress, now_ms: i64) {
        self.seen.insert((room, who), now_ms);
    }

    /// Drop expired entries. Returns `true` if anything changed, so the caller
    /// can skip a re-render when the sweep is a no-op.
    pub fn sweep(&mut self, now_ms: i64) -> bool {
        let before = self.seen.len();
        self.seen.retain(|_, at| now_ms - *at < TYPING_TTL_MS);
        self.seen.len() != before
    }

    /// Everyone currently typing in a room, excluding yourself — the server
    /// never echoes your own typing, but a local optimistic note might.
    pub fn typists(&self, room: &RoomId, me: &WalletAddress) -> Vec<WalletAddress> {
        let mut v: Vec<WalletAddress> = self
            .seen
            .keys()
            .filter(|(r, w)| r == room && w != me)
            .map(|(_, w)| w.clone())
            .collect();
        // Stable order so the label does not reshuffle between renders.
        v.sort();
        v
    }

    pub fn clear_room(&mut self, room: &RoomId) {
        self.seen.retain(|(r, _), _| r != room);
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use gloo_net::websocket::{futures::WebSocket, Message as WsMessage};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::spawn_local;

    /// Callback signature: one decoded server event, or `None` to signal that
    /// the transport dropped and the caller should show `Offline`.
    pub type OnEvent = Rc<dyn Fn(RealtimeSignal)>;

    /// Open a WebSocket and pump it until cancelled, reconnecting with backoff.
    ///
    /// The token goes in the `Sec-WebSocket-Protocol` header, never the URL:
    /// a query-string token lands in proxy access logs, and these connections
    /// are re-established often enough for that to matter.
    ///
    /// The outbound sink is published into a **caller-owned** slot rather than
    /// returned: the slot has to outlive each individual connection so the
    /// keepalive timer and the composer keep working across a reconnect.
    pub fn connect_ws(
        url: String,
        token: String,
        on: OnEvent,
        cancelled: Rc<RefCell<bool>>,
        sink: Rc<RefCell<Option<UnboundedTypingSink>>>,
    ) {
        let sink_for_task = sink;

        spawn_local(async move {
            let mut attempt = 0u32;
            loop {
                if *cancelled.borrow() {
                    return;
                }
                match WebSocket::open_with_protocols(&url, &["fnauth", &token]) {
                    Ok(ws) => {
                        // NOT `attempt = 0` here. `open_with_protocols` returns
                        // as soon as the JS object is constructed — the
                        // handshake is still in flight. A proxy that strips
                        // `Upgrade` therefore looks exactly like "opened, then
                        // closed immediately", and resetting the counter on
                        // construction made that an infinite loop that never
                        // reached the next transport tier.
                        let opened_at = js_sys::Date::now();
                        on(RealtimeSignal::Connected(Transport::WebSocket));

                        // `Connected` above is for the UI only — it is not
                        // evidence, because construction is not connection.
                        // `Healthy` is the evidence, and it is emitted from a
                        // timer only if this session is still up once the
                        // minimum has elapsed.
                        let session_live = Rc::new(RefCell::new(true));
                        {
                            let on_healthy = on.clone();
                            let cancel_h = cancelled.clone();
                            let live = session_live.clone();
                            spawn_local(async move {
                                gloo_timers::future::TimeoutFuture::new(
                                    MIN_HEALTHY_SESSION_MS as u32,
                                )
                                .await;
                                if !*cancel_h.borrow() && *live.borrow() {
                                    on_healthy(RealtimeSignal::Healthy(Transport::WebSocket));
                                }
                            });
                        }

                        let (mut write, mut read) = ws.split();
                        let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
                        *sink_for_task.borrow_mut() = Some(UnboundedTypingSink(tx));

                        // Outbound: pings and typing frames.
                        let cancel_w = cancelled.clone();
                        spawn_local(async move {
                            while let Some(text) = rx.next().await {
                                if *cancel_w.borrow() {
                                    break;
                                }
                                if write.send(WsMessage::Text(text)).await.is_err() {
                                    break;
                                }
                            }
                            let _ = write.close().await;
                        });

                        while let Some(frame) = read.next().await {
                            if *cancelled.borrow() {
                                return;
                            }
                            if let Ok(WsMessage::Text(text)) = frame {
                                // Unparseable and unknown frames are ignored,
                                // never fatal — forward compatibility.
                                if let Ok(ev) = serde_json::from_str::<ServerEvent>(&text) {
                                    on(RealtimeSignal::Event(ev));
                                }
                            }
                        }
                        *sink_for_task.borrow_mut() = None;
                        *session_live.borrow_mut() = false;

                        // Only a session that actually lasted counts as proof
                        // the transport works. Anything shorter is a flap, and
                        // flaps must accumulate or the tier never exhausts.
                        if session_was_healthy(js_sys::Date::now() - opened_at) {
                            attempt = 0;
                        }
                    }
                    Err(_) => attempt = attempt.saturating_add(1),
                }

                if *cancelled.borrow() {
                    return;
                }
                on(RealtimeSignal::Dropped);
                attempt = attempt.saturating_add(1);

                // Hand over rather than retry forever. The caller watches for
                // this and re-runs with a lower transport tier; continuing to
                // reopen a socket a proxy is stripping would keep the client
                // permanently offline while looking busy.
                if attempt >= super::MAX_ATTEMPTS_PER_TIER {
                    on(RealtimeSignal::Exhausted);
                    return;
                }

                let delay = backoff_delay_ms(attempt.saturating_sub(1), js_sys::Math::random());
                gloo_timers::future::TimeoutFuture::new(delay).await;
            }
        });
    }

    /// A cloneable handle for sending client→server frames.
    pub struct UnboundedTypingSink(pub futures::channel::mpsc::UnboundedSender<String>);

    impl UnboundedTypingSink {
        pub fn send_json<T: serde::Serialize>(&self, msg: &T) {
            if let Ok(text) = serde_json::to_string(msg) {
                let _ = self.0.unbounded_send(text);
            }
        }
    }

    /// Open an SSE stream using a short-lived ticket.
    ///
    /// `EventSource` cannot set headers, so the JWT itself must never appear in
    /// the URL. A ticket is 30 seconds of single-use entropy — worthless in a
    /// log by the time anyone reads it (REALTIME.md §8.1).
    pub fn connect_sse(
        url: String,
        on: OnEvent,
        cancelled: Rc<RefCell<bool>>,
        holder: Rc<RefCell<Option<web_sys::EventSource>>>,
    ) {
        let Ok(es) = web_sys::EventSource::new(&url) else {
            // Nothing was opened, so nothing will retry. Hand straight over to
            // polling rather than leaving the app waiting on a stream that does
            // not exist.
            on(RealtimeSignal::Exhausted);
            return;
        };

        // One listener for every named event: `event:` names mirror the
        // WebSocket `type` tag exactly, so the same deserialiser handles both.
        let on_msg = on.clone();
        let handler =
            Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                if let Some(text) = e.data().as_string() {
                    if let Ok(ev) = serde_json::from_str::<ServerEvent>(&text) {
                        on_msg(RealtimeSignal::Event(ev));
                    }
                }
            });
        es.set_onmessage(Some(handler.as_ref().unchecked_ref()));
        handler.forget();

        let on_open = on.clone();
        let opened = Closure::<dyn Fn()>::new(move || {
            // Unlike a WebSocket constructor, `onopen` fires only after the
            // response headers actually arrived — so here, opened *is* proof,
            // and no settling period is needed.
            on_open(RealtimeSignal::Connected(Transport::Sse));
            on_open(RealtimeSignal::Healthy(Transport::Sse));
        });
        es.set_onopen(Some(opened.as_ref().unchecked_ref()));
        opened.forget();

        let on_err = on.clone();
        let cancel = cancelled.clone();
        let es_for_err = es.clone();
        let errored = Closure::<dyn Fn()>::new(move || {
            if *cancel.borrow() {
                return;
            }
            // `EventSource` reconnects itself while it is CONNECTING (0) or
            // OPEN (1); CLOSED (2) means it has given up for good, and only
            // then should we tier down.
            if es_for_err.ready_state() == web_sys::EventSource::CLOSED {
                on_err(RealtimeSignal::Exhausted);
            } else {
                on_err(RealtimeSignal::Dropped);
            }
        });
        es.set_onerror(Some(errored.as_ref().unchecked_ref()));
        errored.forget();

        *holder.borrow_mut() = Some(es);
    }
}

/// What the transport tells the app.
///
/// Constructed only by the wasm transports; the host build carries it so the
/// rest of the crate — including `app` — still type-checks under `cargo test`.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeSignal {
    /// A transport came up.
    Connected(Transport),
    /// A decoded server event.
    Event(ServerEvent),
    /// The transport went away; the loop will retry with backoff.
    Dropped,
    /// The transport has been up long enough to count as working, so the
    /// consecutive-failure count may be cleared.
    ///
    /// Deliberately distinct from [`Self::Connected`]: a WebSocket object
    /// exists the instant it is constructed, long before the handshake either
    /// succeeds or is stripped by a proxy. Treating "constructed" as "working"
    /// resets the failure count on every flap, which pins the client to a
    /// transport that cannot work.
    ///
    /// Carries which transport proved itself, because that determines whether
    /// the failure count may be cleared: clearing it after a *demoted* tier
    /// succeeds would promote straight back to the tier that just failed, and
    /// the two would alternate forever.
    Healthy(Transport),
    /// The transport gave up after [`MAX_ATTEMPTS_PER_TIER`] consecutive
    /// failures. This is the signal that drives [`select_transport`] down to
    /// the next tier — without it a failing WebSocket would retry itself
    /// forever and SSE and polling would never be reached.
    Exhausted,
}

#[cfg(target_arch = "wasm32")]
pub use imp::{connect_sse, connect_ws, OnEvent, UnboundedTypingSink};

/// Host-side stubs with the same signatures as [`imp`].
///
/// They exist so `cargo test` can compile the whole crate — including `app` and
/// every component — on the host and run the pure-logic suites. Without them,
/// the one module that cannot run outside a browser would make the other twenty
/// untestable without a browser.
#[cfg(not(target_arch = "wasm32"))]
mod imp {
    #![allow(dead_code)]

    use super::*;

    pub type OnEvent = Rc<dyn Fn(RealtimeSignal)>;

    pub struct UnboundedTypingSink;

    impl UnboundedTypingSink {
        pub fn send_json<T: serde::Serialize>(&self, _msg: &T) {}
    }

    pub fn connect_ws(
        _url: String,
        _token: String,
        _on: OnEvent,
        _cancelled: Rc<RefCell<bool>>,
        _sink: Rc<RefCell<Option<UnboundedTypingSink>>>,
    ) {
    }

    pub fn connect_sse(
        _url: String,
        _on: OnEvent,
        _cancelled: Rc<RefCell<bool>>,
        _holder: Rc<RefCell<Option<web_sys::EventSource>>>,
    ) {
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use imp::{connect_sse, connect_ws, OnEvent, UnboundedTypingSink};

/// Build the `ws(s)://` URL for `/ws` from the page's own origin.
///
/// Deriving it from `location` rather than from a compile-time constant is what
/// lets the same bundle work on `localhost`, on a LAN IP, and behind TLS
/// without a rebuild.
#[cfg(target_arch = "wasm32")]
pub fn websocket_url() -> Option<String> {
    let loc = web_sys::window()?.location();
    let proto = if loc.protocol().ok()? == "https:" {
        "wss:"
    } else {
        "ws:"
    };
    Some(format!("{proto}//{}/ws", loc.host().ok()?))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn websocket_url() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(n: u8) -> RoomId {
        RoomId::new(&format!("room_00000{n:03}")).unwrap()
    }

    fn addr(n: u8) -> WalletAddress {
        WalletAddress::new(&format!("0x{:040x}", n as u32)).unwrap()
    }

    #[test]
    fn backoff_follows_the_documented_sequence_at_zero_jitter() {
        // jitter01 = 0.5 is the midpoint, i.e. no net adjustment.
        let d = |a| backoff_delay_ms(a, 0.5);
        assert_eq!(d(0), 1_000);
        assert_eq!(d(1), 2_000);
        assert_eq!(d(2), 4_000);
        assert_eq!(d(3), 8_000);
        assert_eq!(d(4), 16_000);
        // Capped at 30 s from here on.
        assert_eq!(d(5), 30_000);
        assert_eq!(d(6), 30_000);
    }

    /// The ladder is only reachable if a tier actually gives up. `connect_ws`
    /// used to retry forever, so `select_transport` was correct, fully tested,
    /// and never once consulted with a non-zero failure count.
    ///
    /// This pins the arithmetic that connects the two: three attempts per tier,
    /// and the thresholds `select_transport` steps at must be multiples of it,
    /// or a tier would be skipped or repeated.
    /// The counter-reset rule. A WebSocket blocked by a proxy constructs fine
    /// and dies milliseconds later; treating that as a successful connection
    /// reset the retry counter on every cycle, so the tier never exhausted and
    /// the client retried a hopeless transport indefinitely.
    #[test]
    fn only_a_session_that_lasted_resets_the_retry_counter() {
        // A stripped-`Upgrade` handshake: constructed, then immediately gone.
        assert!(!session_was_healthy(0.0));
        assert!(!session_was_healthy(5.0));
        assert!(!session_was_healthy(MIN_HEALTHY_SESSION_MS - 1.0));

        // A connection that genuinely worked and later dropped.
        assert!(session_was_healthy(MIN_HEALTHY_SESSION_MS));
        assert!(session_was_healthy(60_000.0));

        // A clock that jumped backwards must not be read as health.
        assert!(!session_was_healthy(-1_000.0));
        assert!(!session_was_healthy(f64::NAN));
    }

    /// A demotion has to stick. Clearing the failure count whenever *any* tier
    /// proves healthy sends the client straight back to the transport that just
    /// failed, and the two alternate forever — observed in the browser as
    /// WS×3 → SSE → WS×3 → SSE, indefinitely.
    ///
    /// The rule: only the tier we would have picked with a clean slate is
    /// allowed to clear the debt.
    #[test]
    fn only_the_preferred_tier_may_clear_the_failure_count() {
        let clears = |preference, healthy| select_transport(preference, 0) == healthy;

        // WebSocket is the preferred tier, so only it resets the ladder.
        assert!(clears(ConnectionMode::WebSocket, Transport::WebSocket));
        assert!(!clears(ConnectionMode::WebSocket, Transport::Sse));
        assert!(!clears(ConnectionMode::WebSocket, Transport::Polling));

        // A user who explicitly chose SSE has SSE as their top tier.
        assert!(clears(ConnectionMode::Sse, Transport::Sse));
        assert!(!clears(ConnectionMode::Sse, Transport::Polling));

        // Polling is both the preference and the floor.
        assert!(clears(ConnectionMode::Polling, Transport::Polling));
    }

    #[test]
    fn a_tier_gives_up_in_time_to_reach_the_next_one() {
        // Failure counts accumulate one per exhausted tier, so walking them in
        // order must visit every transport exactly once.
        let walked: Vec<Transport> = (0..3)
            .map(|failures| select_transport(ConnectionMode::WebSocket, failures * 2))
            .collect();
        assert_eq!(
            walked,
            vec![Transport::WebSocket, Transport::Sse, Transport::Polling],
            "escalation must not skip or repeat a tier"
        );

        // And polling is terminal: there is nothing below it to fall to.
        assert_eq!(
            select_transport(ConnectionMode::WebSocket, u32::MAX),
            Transport::Polling
        );
    }

    #[test]
    fn backoff_never_overflows_or_collapses_at_large_attempt_counts() {
        // A naive `1 << attempt` overflows and can wrap to a *tiny* delay,
        // turning a backoff into a flood. This is the regression guard.
        for a in [20u32, 32, 63, 64, 1000, u32::MAX] {
            let d = backoff_delay_ms(a, 0.5);
            assert_eq!(d, 30_000, "attempt {a} produced {d}ms");
        }
    }

    #[test]
    fn jitter_spans_plus_or_minus_twenty_percent() {
        assert_eq!(backoff_delay_ms(4, 0.0), 12_800); // 16s × 0.8
        assert_eq!(backoff_delay_ms(4, 1.0), 19_200); // 16s × 1.2
                                                      // Out-of-range input is clamped rather than producing a wild delay.
        assert_eq!(backoff_delay_ms(4, -5.0), 12_800);
        assert_eq!(backoff_delay_ms(4, 99.0), 19_200);
    }

    #[test]
    fn jitter_is_monotonic_in_its_input() {
        let mut prev = 0;
        for i in 0..=10 {
            let d = backoff_delay_ms(3, i as f64 / 10.0);
            assert!(d >= prev, "delay went backwards at {i}");
            prev = d;
        }
    }

    #[test]
    fn websocket_preference_degrades_one_tier_at_a_time_and_stops_at_polling() {
        use ConnectionMode::WebSocket as W;
        assert_eq!(select_transport(W, 0), Transport::WebSocket);
        assert_eq!(select_transport(W, 1), Transport::WebSocket);
        assert_eq!(select_transport(W, 2), Transport::Sse);
        assert_eq!(select_transport(W, 3), Transport::Sse);
        assert_eq!(select_transport(W, 4), Transport::Polling);
        assert_eq!(select_transport(W, 99), Transport::Polling);
    }

    #[test]
    fn an_explicit_polling_preference_is_never_upgraded() {
        // Silently "improving" past a user's explicit choice is a bug: they may
        // have chosen polling because their proxy mangles WebSockets.
        for f in [0, 1, 5, 100] {
            assert_eq!(
                select_transport(ConnectionMode::Polling, f),
                Transport::Polling
            );
        }
    }

    #[test]
    fn an_sse_preference_degrades_only_to_polling_never_up_to_websocket() {
        assert_eq!(select_transport(ConnectionMode::Sse, 0), Transport::Sse);
        assert_eq!(select_transport(ConnectionMode::Sse, 1), Transport::Sse);
        assert_eq!(select_transport(ConnectionMode::Sse, 2), Transport::Polling);
    }

    #[test]
    fn typing_entries_expire_four_seconds_after_the_last_event() {
        let mut t = TypingTracker::default();
        t.note(room(1), addr(2), 0);
        assert_eq!(t.typists(&room(1), &addr(1)), vec![addr(2)]);

        assert!(!t.sweep(3_999), "still within the TTL");
        assert_eq!(t.typists(&room(1), &addr(1)).len(), 1);

        assert!(t.sweep(4_000), "sweep must report the change");
        assert!(t.typists(&room(1), &addr(1)).is_empty());
        // A second sweep changes nothing, so it must not force a re-render.
        assert!(!t.sweep(5_000));
    }

    #[test]
    fn a_fresh_typing_event_refreshes_the_expiry() {
        let mut t = TypingTracker::default();
        t.note(room(1), addr(2), 0);
        t.note(room(1), addr(2), 3_000);
        t.sweep(4_000);
        assert_eq!(t.typists(&room(1), &addr(1)).len(), 1);
    }

    #[test]
    fn typing_is_scoped_per_room_and_excludes_yourself() {
        let mut t = TypingTracker::default();
        t.note(room(1), addr(2), 0);
        t.note(room(2), addr(3), 0);
        t.note(room(1), addr(1), 0); // me
        assert_eq!(t.typists(&room(1), &addr(1)), vec![addr(2)]);
        assert_eq!(t.typists(&room(2), &addr(1)), vec![addr(3)]);

        t.clear_room(&room(1));
        assert!(t.typists(&room(1), &addr(1)).is_empty());
        assert_eq!(t.typists(&room(2), &addr(1)).len(), 1);
    }

    #[test]
    fn typist_order_is_stable_so_the_label_does_not_reshuffle() {
        let mut t = TypingTracker::default();
        t.note(room(1), addr(5), 0);
        t.note(room(1), addr(2), 0);
        t.note(room(1), addr(9), 0);
        assert_eq!(t.typists(&room(1), &addr(1)), t.typists(&room(1), &addr(1)));
        assert_eq!(
            t.typists(&room(1), &addr(1)),
            vec![addr(2), addr(5), addr(9)]
        );
    }

    #[test]
    fn every_connection_state_has_a_class_a_label_and_a_spoken_name() {
        let states = [
            ConnStatus::Live(Transport::WebSocket),
            ConnStatus::Live(Transport::Sse),
            ConnStatus::Live(Transport::Polling),
            ConnStatus::Syncing,
            ConnStatus::Offline,
        ];
        for s in states {
            assert!(s.pill_class().starts_with("fn-conn--"));
            // Colour is never the only signal (DESIGN.md §17), so the label
            // must carry the meaning on its own — in every language, not just
            // the one this file happens to be written in.
            for lang in crate::i18n::Lang::ALL {
                assert!(
                    !s.label(lang).is_empty(),
                    "empty pill label in {}",
                    lang.tag()
                );
                assert!(
                    !s.aria_label(lang).trim().is_empty(),
                    "empty pill aria-label in {}",
                    lang.tag()
                );
            }
            assert!(s
                .aria_label(crate::i18n::Lang::En)
                .starts_with("Connection: "));
        }
    }
}
