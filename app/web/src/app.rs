//! The root component: routing, the auth gate, the realtime lifecycle and the
//! modal layer.
//!
//! This is the only place that touches `history`, `navigator.onLine` or the
//! realtime transport. Everything below it is a pure function of the store.

use std::cell::RefCell;
use std::rc::Rc;

use pocketskynet_core::{ClientMessage, PresenceStatus, RoomId, ServerEvent};
use yew::prelude::*;

use crate::actions;
use crate::api::Client;
use crate::components::{
    bank, boot, burst, chat, dialogs, invitations, knowledge, lightbox, login, members, operator,
    publish, room_list, settings, shell, shout, spotlight, toast, transfers,
};
use crate::format;
use crate::i18n::{t, Key};
use crate::realtime::{self, ConnStatus, RealtimeSignal, Transport};
use crate::route::Route;
use crate::session::{Auth, ConnectionMode, Theme};
use crate::state::{Action, AppState, ConfirmAction, Modal, Store};

/// How often a client with no upstream channel repeats its status.
///
/// Comfortably inside the server's `BEACON_TTL_MS` (150 s), because a
/// background tab's timers are throttled rather than stopped: a cadence any
/// closer to the window would let ordinary throttling read as "gone".
const PRESENCE_BEACON_MS: u32 = 60_000;

/// Tell the server this client stepped away, or came back.
///
/// Over the socket when there is one — it costs a frame the connection is
/// already carrying and needs no round trip — and over REST otherwise. Two
/// paths rather than one because the cheap path does not exist on two of the
/// three transports, and the expensive one would be a request per tab switch
/// per person on the tier where most people actually are.
fn declare(
    store: &Store,
    sink: &Rc<RefCell<Option<realtime::UnboundedTypingSink>>>,
    status: PresenceStatus,
) {
    if let Some(sink) = sink.borrow().as_ref() {
        sink.send_json(&ClientMessage::Presence { status });
        return;
    }
    wasm_bindgen_futures::spawn_local(actions::declare_presence(store.clone(), status));
}

#[function_component(App)]
pub fn app() -> Html {
    // Restore whatever survived the last page load. Never `Unlocked` — keys are
    // not persisted, so a reload always lands in `Locked` or `SignedOut`.
    let store = use_reducer(|| {
        let auth = Auth::restore();
        let client = Client::default().with_token(auth.token());
        let mut s = AppState::new(auth, client);
        s.theme = Theme::load();
        s.mode = ConnectionMode::load();
        s
    });

    html! {
        <ContextProvider<Store> context={store.clone()}>
            <Root />
        </ContextProvider<Store>>
    }
}

#[function_component(Root)]
fn root() -> Html {
    let store = crate::state::use_store();
    let route = use_state(|| Route::parse(&current_path()));

    // --- navigation -------------------------------------------------------

    let navigate = {
        let route = route.clone();
        Callback::from(move |next: Route| {
            push_state(&next);
            set_title(next.title());
            route.set(next);
        })
    };

    // Browser back/forward.
    {
        let route = route.clone();
        use_effect_with((), move |_| {
            let listener = gloo_events::EventListener::new(
                &web_sys::window().expect("a browser window"),
                "popstate",
                move |_| route.set(Route::parse(&current_path())),
            );
            move || drop(listener)
        });
    }

    // Apply the persisted theme, language and type preferences once, before
    // first paint settles.
    {
        let theme = store.theme;
        let skin = store.skin;
        let language = store.language;
        let font_face = store.font_face;
        let font_scale = store.font_scale;
        use_effect_with((), move |_| {
            theme.apply();
            skin.apply();
            // Stamps `<html lang>`, which is what a screen reader keys its
            // voice off — not merely a preference echo.
            language.apply();
            font_face.apply();
            font_scale.apply();
            || ()
        });
    }

    // --- unlock from the device vault --------------------------------------

    // A reload almost never lands on `/login`; it lands on the room the user
    // was reading. `Login` never mounts there, so without this the session
    // would sit **locked** — sealed bubbles next to a perfectly good JWT — even
    // though this device was told to remember the credential.
    //
    // Skipped on `/login` itself, where the login screen runs the same unlock
    // with a cutscene and a form to fall back on; two of them would mean two
    // challenge round trips against a 5-logins-per-minute limiter.
    {
        let store = store.clone();
        let on_login_screen = matches!(*route, Route::Login);
        use_effect_with(
            (store.auth.can_decrypt(), on_login_screen),
            move |(can_decrypt, on_login_screen)| {
                if !*can_decrypt && !*on_login_screen {
                    wasm_bindgen_futures::spawn_local(actions::unlock_from_vault(store));
                }
                || ()
            },
        );
    }

    // --- report in for the day ---------------------------------------------

    // Keyed on *having* a token rather than on mount: a reload lands here with
    // a session already restored, and the streak should count the day the
    // operator actually used the app, not the number of times it was opened.
    // `boot` is idempotent within a day, so re-running it costs nothing.
    {
        let signed_in = store.auth.token().is_some();
        use_effect_with(signed_in, move |signed_in| {
            if *signed_in {
                crate::progression::boot();
            }
            || ()
        });
    }

    // --- connectivity -----------------------------------------------------

    {
        let store = store.clone();
        // Keyed on the token for the same reason as the sync interval below:
        // the "back online" handler calls `refresh_all`, which uses
        // `store.client`. Captured before sign-in, that client has no JWT, so a
        // network flap after logging in would 401 and sign the user out.
        use_effect_with(store.auth.token().map(str::to_owned), move |_| {
            let window = web_sys::window().expect("a browser window");

            // `navigator.onLine` is not the question. It reports whether the
            // machine has a network route, and this server is routinely on
            // loopback, on the LAN, or on a mesh VPN — every one of which
            // answers with the Wi-Fi off, when that flag reads `false`. Asking
            // the server directly is the only signal that means what the rest
            // of the app assumes `online` means.
            //
            // The browser's events are kept, demoted to hints: they say
            // "something about the network just changed, look again" rather
            // than what the answer is.

            // What the store was last told. Read back from `store` instead and
            // it would be whatever this render captured — the effect runs once,
            // so that value never updates and every probe would look like a
            // change.
            let last_up = Rc::new(std::cell::Cell::new(true));

            let probe = {
                let store = store.clone();
                let last_up = last_up.clone();
                move |announce: bool| {
                    let store = store.clone();
                    let last_up = last_up.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let up = store.client.reachable().await;
                        // Only a transition is worth acting on: no re-toasting
                        // and no re-syncing while the answer stays the same.
                        if up == last_up.get() {
                            return;
                        }
                        last_up.set(up);
                        store.dispatch(Action::SetOnline(up));
                        if up {
                            store.dispatch(Action::SetConn(ConnStatus::Syncing));
                            if announce {
                                toast::info(&store, t(store.language, Key::back_online));
                            }
                            wasm_bindgen_futures::spawn_local(actions::refresh_all(store.clone()));
                        }
                    });
                }
            };

            // The store starts `online: true` so the first paint is usable; this
            // corrects it if the server really is not there. Nothing to announce
            // yet — nobody has seen a connected state to be told they lost.
            probe(false);

            let up = {
                let probe = probe.clone();
                gloo_events::EventListener::new(&window, "online", move |_| probe(true))
            };
            let down = {
                let probe = probe.clone();
                gloo_events::EventListener::new(&window, "offline", move |_| probe(true))
            };

            // Coming back has no event to wait for. A slept laptop, a restarted
            // server and a dropped VPN all look identical from here and none of
            // them fire `online`, so without a retry the app would sit in a
            // state it can never leave — which is the dead-button failure this
            // whole effect exists to avoid. Only while down: once the server
            // answers, this stops asking.
            let poll = gloo_timers::callback::Interval::new(5_000, {
                let last_up = last_up.clone();
                move || {
                    if !last_up.get() {
                        probe(true);
                    }
                }
            });

            move || {
                drop(up);
                drop(down);
                drop(poll);
            }
        });
    }

    // --- initial load -----------------------------------------------------

    {
        let store = store.clone();
        let token = store.auth.token().map(str::to_owned);
        use_effect_with(token, move |token| {
            let signed_in = token.is_some();
            {
                let store = store.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    // Chain info first: the testnet ribbon and every explorer
                    // link depend on it, and it is unauthenticated and cheap.
                    //
                    // Fetched **whether or not there is a token**. It used to be
                    // gated on being signed in, which left the login screen with
                    // an empty `chain`: the Privy button was never offered
                    // because its app id lives here, and the MetaMask button had
                    // no chain id to ask the wallet to switch to. Both failed
                    // silently, which is the worst way for a login option to be
                    // missing.
                    if let Ok(info) = store.client.blockchain_info().await {
                        store.dispatch(Action::SetChain(info));
                    }
                    // The multi-chain registry for the wallet. Same reasoning:
                    // unauthenticated, cheap, and the wallet button is dead
                    // until it arrives.
                    if let Ok(nets) = store.client.networks().await {
                        store.dispatch(Action::SetNetworks(nets));
                    }
                    // Everything past here needs a session.
                    if !signed_in {
                        return;
                    }
                    actions::refresh_all(store).await;
                });
            }
            || ()
        });
    }

    // --- realtime ---------------------------------------------------------

    // The outbound half of the socket, published by `connect_ws` once the
    // handshake completes.
    //
    // `use_mut_ref`, **not** `use_state`: a `UseStateHandle` captured by an
    // effect is a snapshot of that render, so a later `set` would be invisible
    // to the keepalive timer — it would ping into the empty slot it captured on
    // the first render, forever. A mut ref is one stable cell across renders.
    let sink_slot: Rc<RefCell<Option<realtime::UnboundedTypingSink>>> = use_mut_ref(|| None);

    // Consecutive tier failures. It is part of the effect key on purpose: when
    // a transport gives up, this changes, the effect tears the old transport
    // down and re-runs, and `select_transport` picks the next tier. Passing a
    // constant here — as this once did — makes the whole WS → SSE → polling
    // ladder unreachable, because a failing WebSocket just retries itself.
    let tier_failures = use_state(|| 0u32);

    {
        let store = store.clone();
        let sink_slot = sink_slot.clone();
        let tier_failures = tier_failures.clone();
        let key = (
            store.auth.token().map(str::to_owned),
            store.mode,
            *tier_failures,
        );
        use_effect_with(key, move |(token, mode, failures)| {
            let failures = *failures;
            let cancelled = Rc::new(RefCell::new(false));
            let source: Rc<RefCell<Option<web_sys::EventSource>>> = Rc::new(RefCell::new(None));
            let Some(token) = token.clone() else {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };

            let on_signal: realtime::OnEvent = {
                let store = store.clone();
                let tier_failures = tier_failures.clone();
                let preferred = *mode;
                Rc::new(move |signal: RealtimeSignal| match signal {
                    RealtimeSignal::Connected(t) => {
                        store.dispatch(Action::SetConn(ConnStatus::Live(t)))
                    }
                    RealtimeSignal::Healthy(t) => {
                        // The moment the hole gets filled. Presence events are
                        // transient and never replayed, so a client that was
                        // away from the network cannot learn what it missed by
                        // waiting — nothing re-announces a status that has not
                        // changed. `Healthy` rather than `Connected` because a
                        // WebSocket object exists before its handshake does,
                        // and a failing tier would otherwise fetch on every
                        // retry.
                        wasm_bindgen_futures::spawn_local(actions::refresh_presence(store.clone()));
                        // Clear the debt only when the *preferred* tier is the
                        // one that proved itself. Clearing it after a demoted
                        // tier succeeds would immediately promote back to the
                        // transport that just failed, and the two would
                        // alternate for the life of the session.
                        //
                        // So a demotion is sticky: once SSE works behind a
                        // proxy that eats WebSockets, we stay on SSE until the
                        // user changes preference or reloads.
                        if realtime::select_transport(preferred, 0) == t && *tier_failures != 0 {
                            tier_failures.set(0);
                        }
                    }
                    RealtimeSignal::Dropped => store.dispatch(Action::SetConn(ConnStatus::Offline)),
                    RealtimeSignal::Exhausted => {
                        store.dispatch(Action::SetConn(ConnStatus::Offline));
                        // Re-keys the effect: tear this transport down and come
                        // back on the next tier.
                        tier_failures.set(tier_failures.saturating_add(1));
                    }
                    RealtimeSignal::Event(ev) => handle_event(&store, ev),
                })
            };

            match realtime::select_transport(*mode, failures) {
                Transport::WebSocket => {
                    if let Some(url) = realtime::websocket_url() {
                        realtime::connect_ws(
                            url,
                            token,
                            on_signal,
                            cancelled.clone(),
                            sink_slot.clone(),
                        );
                    }
                }
                Transport::Sse => {
                    // `EventSource` cannot carry a bearer header, so the stream
                    // is authenticated with a single-use ticket. If the server
                    // has no ticket endpoint we degrade to polling rather than
                    // putting the JWT in a URL.
                    let store = store.clone();
                    let holder = source.clone();
                    let cancelled = cancelled.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        match store.client.events_ticket().await {
                            Ok(ticket) => realtime::connect_sse(
                                store.client.url(&format!(
                                    "/api/events?ticket={}",
                                    crate::api::encode_query(&ticket)
                                )),
                                on_signal,
                                cancelled,
                                holder,
                            ),
                            Err(_) => store
                                .dispatch(Action::SetConn(ConnStatus::Live(Transport::Polling))),
                        }
                    });
                }
                Transport::Polling => {
                    // No socket at all: the timer below carries the load.
                    store.dispatch(Action::SetConn(ConnStatus::Live(Transport::Polling)));
                }
            }

            Box::new(move || {
                *cancelled.borrow_mut() = true;
                if let Some(es) = source.borrow_mut().take() {
                    es.close();
                }
            })
        });
    }

    // Keepalive: an app-level ping under the server's 30 s tick, which also
    // zeroes its missed-ping counter.
    {
        let sink_slot = sink_slot.clone();
        use_effect_with((), move |_| {
            let interval =
                gloo_timers::callback::Interval::new(realtime::PING_INTERVAL_MS, move || {
                    if let Some(sink) = sink_slot.borrow().as_ref() {
                        sink.send_json(&ClientMessage::Ping);
                    }
                });
            move || drop(interval)
        });
    }

    // Polling / safety-net sync. Runs in every mode: on a healthy socket it is
    // a slow backstop against a dropped wake-up; in polling mode it *is* the
    // transport.
    {
        let store = store.clone();
        let room = route.room_id().cloned();
        let period = if store.mode == ConnectionMode::Polling {
            realtime::POLL_INTERVAL_MS
        } else {
            realtime::SAFETY_SYNC_MS
        };
        // The token is part of the key, and must stay part of it. `store` is a
        // per-render snapshot and `store.client` carries the JWT, so an interval
        // created before sign-in captures a *tokenless* client and keeps it
        // forever. The first tick then 401s, and the 401 handler signs the user
        // out — i.e. logging in and waiting a minute logged you straight back
        // out. Re-keying on the token rebuilds the interval at sign-in.
        use_effect_with(
            (
                room,
                period,
                store.online,
                store.auth.token().map(str::to_owned),
            ),
            move |(room, period, online, _token)| {
                if !*online {
                    return Box::new(|| ()) as Box<dyn FnOnce()>;
                }
                let room = room.clone();
                let interval = gloo_timers::callback::Interval::new(*period, move || {
                    let store = store.clone();
                    let room = room.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Some(id) = room {
                            let from = store.room_state(&id).map(|s| s.cursor).unwrap_or(0);
                            actions::drain_sync(store.clone(), id, from).await;
                        }
                        // Shouts too: in polling mode this interval is the
                        // only way a paid broadcast ever reaches the screen.
                        actions::refresh_shouts(store.clone()).await;
                        actions::refresh_rooms(store).await;
                    });
                });
                Box::new(move || drop(interval))
            },
        );
    }

    // --- presence ---------------------------------------------------------

    // The one thing the server cannot see for itself: this tab went to the
    // background, or came back.
    //
    // Without it the server has only an idle timer, and an idle timer gets both
    // ends of this wrong — somebody reading a long thread looks away after five
    // minutes, and somebody who shut the lid looks present until their socket
    // finally dies. `visibilitychange` is the ground truth, and it is the event
    // browsers fire *before* they start throttling the timers that would
    // otherwise have been the only clue.
    {
        let store = store.clone();
        let sink_slot = sink_slot.clone();
        use_effect_with(store.auth.token().map(str::to_owned), move |token| {
            if token.is_none() {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            let Some(document) = web_sys::window().and_then(|w| w.document()) else {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            };

            let listener = gloo_events::EventListener::new(&document, "visibilitychange", {
                let store = store.clone();
                let sink_slot = sink_slot.clone();
                move |_| {
                    let hidden = web_sys::window()
                        .and_then(|w| w.document())
                        .map(|d| d.hidden())
                        .unwrap_or(false);
                    let status = if hidden {
                        PresenceStatus::Away
                    } else {
                        PresenceStatus::Online
                    };
                    declare(&store, &sink_slot, status);
                }
            });
            Box::new(move || drop(listener))
        });
    }

    // The heartbeat for the tiers with no upstream channel.
    //
    // A WebSocket client already tells the server it is alive every 25 seconds
    // with its keepalive, and the server counts that as activity. SSE is
    // one-directional and polling holds no connection at all, so without this
    // an SSE reader would age into a false *away* after five minutes and a
    // polling client would never appear at all.
    //
    // Named tiers rather than "not WebSocket", and that distinction is the
    // whole of it. `conn` starts `Offline` and passes through `Syncing`, so the
    // looser test fires once during start-up — before this client knows which
    // transport it will get — and a WebSocket session would open with a beacon
    // it then never repeats or retracts. Beyond being a claim this client
    // cannot back up, it is a claim nothing can *close*: a socket ends when it
    // ends, a beacon only times out, so that stray call kept people lit for two
    // and a half minutes after they shut the lid. Presence is only ever
    // asserted from a transport that is up and needs asserting.
    {
        let store = store.clone();
        let needs_beacon = matches!(
            store.conn,
            ConnStatus::Live(Transport::Sse) | ConnStatus::Live(Transport::Polling)
        );
        use_effect_with(
            (needs_beacon, store.auth.token().map(str::to_owned)),
            move |(needs_beacon, token)| {
                if !*needs_beacon || token.is_none() {
                    return Box::new(|| ()) as Box<dyn FnOnce()>;
                }
                let beat = {
                    let store = store.clone();
                    move || {
                        let store = store.clone();
                        let hidden = web_sys::window()
                            .and_then(|w| w.document())
                            .map(|d| d.hidden())
                            .unwrap_or(false);
                        let status = if hidden {
                            PresenceStatus::Away
                        } else {
                            PresenceStatus::Online
                        };
                        wasm_bindgen_futures::spawn_local(actions::declare_presence(store, status));
                    }
                };
                // Once immediately, or a polling client would be invisible for
                // the first minute of every session — which is most of the time
                // anyone spends looking at whether their colleagues are there.
                beat();
                let interval =
                    gloo_timers::callback::Interval::new(PRESENCE_BEACON_MS, beat.clone());
                Box::new(move || drop(interval))
            },
        );
    }

    // Typing expiry sweep, once a second. Event-driven expiry is impossible:
    // the protocol has no "stopped typing" message.
    {
        let store = store.clone();
        use_effect_with((), move |_| {
            let interval = gloo_timers::callback::Interval::new(1_000, move || {
                store.dispatch(Action::SweepTyping(format::now_ms()));
            });
            move || drop(interval)
        });
    }

    // --- open the selected room ------------------------------------------

    {
        let store = store.clone();
        let room = route.room_id().cloned();
        use_effect_with(
            (room, store.auth.token().map(str::to_owned)),
            move |(room, token)| {
                let leaving = room.clone();
                if let (Some(id), Some(_)) = (room.clone(), token.clone()) {
                    let store2 = store.clone();
                    wasm_bindgen_futures::spawn_local(actions::open_room(store2, id));
                }
                move || {
                    // A typing indicator that outlives the room it belonged to
                    // reappears, stale, the next time you open that room.
                    if let Some(id) = leaving {
                        store.dispatch(Action::ClearTyping(id));
                    }
                }
            },
        );
    }

    // --- render -----------------------------------------------------------

    let on_navigate = navigate.clone();

    // The auth gate. A route that needs a session with none present becomes
    // the login screen rather than a redirect loop.
    if route.needs_auth() && !store.auth.is_authenticated() {
        return html! {
            <>
                <login::Login locked_as={None} on_navigate={on_navigate.clone()} />
                <toast::Toasts />
            </>
        };
    }

    if matches!(*route, Route::Login) || !store.auth.can_decrypt() && needs_unlock(&route) {
        let locked = match &store.auth {
            Auth::Locked(p) => Some((p.wallet_address.clone(), p.username.clone())),
            Auth::Unlocked(s) => Some((s.address().clone(), s.user.username.clone())),
            Auth::SignedOut => None,
        };
        // A locked session can still read plaintext, so only `/login` itself
        // forces the unlock screen; every other route renders normally with
        // sealed bubbles where a key is required.
        if matches!(*route, Route::Login) {
            return html! {
                <>
                    <login::Login locked_as={locked} on_navigate={on_navigate.clone()} />
                    <toast::Toasts />
                </>
            };
        }
    }

    // The vault unlock above verified a session and parked it: play the same
    // boot cutscene a sign-in gets (skippable; collapsed under
    // reduced-motion), then promote it. Without this a reload silently
    // skipped the arrival — the one piece of theatre the product is named for.
    if let Some(session) = store.pending_boot.clone() {
        let store2 = store.clone();
        return html! {
            <boot::BootSequence
                username={session.user.display_name()}
                on_done={Callback::from(move |_: ()| store2.dispatch(Action::FinishBoot))}
            />
        };
    }

    if matches!(*route, Route::NotFound) {
        return html! {
            <>
                <settings::NotFound
                    on_navigate={on_navigate.clone()}
                    authenticated={store.auth.is_authenticated()}
                />
                <toast::Toasts />
            </>
        };
    }

    let list = html! {
        <room_list::RoomList
            selected={route.room_id().cloned()}
            on_navigate={on_navigate.clone()}
            on_reload={{
                let store = store.clone();
                Callback::from(move |_: ()| {
                    let store = store.clone();
                    wasm_bindgen_futures::spawn_local(actions::refresh_all(store));
                })
            }}
        />
    };

    let detail = match &*route {
        Route::Room(id) => html! {
            <chat::Chat
                room_id={id.clone()}
                on_navigate={on_navigate.clone()}
                on_refresh={{
                    let store = store.clone();
                    Callback::from(move |id: RoomId| {
                        let store = store.clone();
                        store.dispatch(Action::SetConn(ConnStatus::Syncing));
                        // The explicit gesture is the one place a full refetch
                        // happens: drops the cached copy and asks the server
                        // for everything again (actions.rs `resync_room`).
                        wasm_bindgen_futures::spawn_local(actions::resync_room(store, id));
                    })
                }}
                on_typing={{
                    let sink_slot = sink_slot.clone();
                    Callback::from(move |id: RoomId| {
                        if let Some(sink) = sink_slot.borrow().as_ref() {
                            sink.send_json(&ClientMessage::Typing { room_id: id });
                        }
                    })
                }}
            />
        },
        Route::Members(id) => html! {
            <members::Members room_id={id.clone()} on_navigate={on_navigate.clone()} />
        },
        Route::Invitations => html! {
            <invitations::Invitations on_navigate={on_navigate.clone()} />
        },
        Route::Knowledge => html! {
            <knowledge::Knowledge on_navigate={on_navigate.clone()} />
        },
        Route::Publish => html! {
            <publish::Publish on_navigate={on_navigate.clone()} />
        },
        Route::Bank => html! { <bank::Bank /> },
        Route::Operator => html! { <operator::OperatorPage store={store.clone()} /> },
        Route::Settings => html! {
            <settings::Settings on_navigate={on_navigate.clone()} />
        },
        _ => html! { <chat::NoRoom /> },
    };

    html! {
        <>
            <shell::Shell
                route={(*route).clone()}
                on_navigate={on_navigate.clone()}
                {list}
                {detail}
            />
            { render_modal(&store, &route, &on_navigate) }
            <toast::Toasts />
            // Transfer progress (transfers.rs). A singleton like the layers
            // below, and load-bearing rather than tidy: a 4 GB upload outlives
            // the screen that started it, so a bar living in the composer
            // would vanish the moment someone opened Settings and the transfer
            // would look like it had stopped.
            <transfers::TransferRail />
            // The particle layer (burst.rs): mounted once, fired from
            // anywhere — the send button's pop, a deleted message's poof.
            <burst::BurstLayer />
            // The portrait spotlight (spotlight.rs): same singleton shape.
            <spotlight::SpotlightLayer />
            // The image lightbox (lightbox.rs): a picture in a room, tapped,
            // taking the screen. Same singleton shape again.
            <lightbox::LightboxLayer />
            // Paid broadcasts (shout.rs): same singleton shape, fed by
            // `actions::refresh_shouts`.
            <shout::ShoutLayer />
        </>
    }
}

/// Whether this route is meaningless without keys. Only `/login` is — every
/// other screen degrades to sealed bubbles rather than blocking.
fn needs_unlock(route: &Route) -> bool {
    matches!(route, Route::Login)
}

/// React to one realtime event (REALTIME.md §6). Every one of these is a
/// **wake-up signal**: the reaction is always "go ask REST", never "render what
/// the socket said", because the socket carries no content and applies no
/// authorisation.
fn handle_event(store: &Store, ev: ServerEvent) {
    match ev {
        ServerEvent::NewMessage { room_id, .. } => {
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let from = store.room_state(&room_id).map(|s| s.cursor).unwrap_or(0);
                actions::drain_sync(store.clone(), room_id.clone(), from).await;
                actions::refresh_rooms(store.clone()).await;

                // A rotation is broadcast as `new_message` too, so the epoch may
                // have advanced. Refetching keys costs an ECDH unwrap per epoch,
                // so do it only when the room's current epoch is one we do not
                // already hold — not on every single incoming message.
                if !store.can_post_encrypted(&room_id) {
                    actions::refresh_keys(store, room_id).await;
                }
            });
        }
        ServerEvent::RoomsUpdated | ServerEvent::MemberRemoved { .. } => {
            // `member_removed` means "roster changed", not literally "someone
            // left" — the server reuses it for accept-invitation too.
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                actions::refresh_rooms(store.clone()).await;
                // A roster change changes who this client may see the presence
                // of, and presence is only announced when it *moves* — a
                // colleague who has been online for an hour will not announce
                // themselves again just because you joined their room.
                actions::refresh_presence(store).await;
            });
        }
        ServerEvent::InvitationReceived { .. } => {
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                actions::refresh_invitations(store.clone()).await;
                toast::info(&store, t(store.language, Key::new_invitation));
            });
        }
        ServerEvent::Typing { room_id, from } => {
            store.dispatch(Action::Typing(room_id, from, format::now_ms()));
        }
        ServerEvent::Presence { wallet, status } => {
            // The one event that is its own payload rather than a wake-up.
            // Authorisation happened before it was sent — it reaches only
            // shared-room members and never crosses a block — and re-reading
            // `GET /api/presence` to learn a single enum would cost a round
            // trip per tab switch across the whole team.
            store.dispatch(Action::Presence(wallet, status));
        }
        ServerEvent::ResyncRequired { .. } => {
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(actions::refresh_all(store));
        }
        ServerEvent::SessionExpired { .. } => {
            toast::error(
                store,
                "Your session expired",
                Some("Sign in again to keep receiving messages.".into()),
            );
            actions::sign_out(store);
        }
        ServerEvent::Shout { .. } => {
            // A wake-up like every other: the banner renders what the REST
            // path returns, never what the socket said.
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(actions::refresh_shouts(store));
        }
        ServerEvent::Pong => {}
    }
}

/// The modal layer. Exactly one dialog can be open, which is what makes the
/// focus trap tractable.
fn render_modal(store: &Store, route: &Route, on_navigate: &Callback<Route>) -> Html {
    let close = {
        let store = store.clone();
        Callback::from(move |_: ()| store.dispatch(Action::CloseModal))
    };

    match store.modal.clone() {
        None => html! {},
        Some(Modal::CreateRoom) => html! {
            <dialogs::CreateRoom
                on_close={close.clone()}
                on_created={{
                    let store = store.clone();
                    let on_navigate = on_navigate.clone();
                    Callback::from(move |id: RoomId| {
                        store.dispatch(Action::CloseModal);
                        on_navigate.emit(Route::Room(id));
                    })
                }}
            />
        },
        Some(Modal::NewDirectMessage) => html! {
            <dialogs::NewDirectMessage
                on_close={close.clone()}
                on_navigate={on_navigate.clone()}
            />
        },
        Some(Modal::Mentions) => html! {
            <dialogs::Mentions
                on_close={close.clone()}
                on_navigate={on_navigate.clone()}
            />
        },
        Some(Modal::AdminConsole) => html! {
            <dialogs::AdminConsole on_close={close.clone()} />
        },
        Some(Modal::Invite(id)) => html! {
            <dialogs::Invite room_id={id} on_close={close} />
        },
        Some(Modal::ManageAdmins(id)) => html! {
            <dialogs::ManageAdmins room_id={id} on_close={close} />
        },
        Some(Modal::Blocked) => html! { <dialogs::Blocked on_close={close} /> },
        Some(Modal::HiddenRooms) => html! { <dialogs::HiddenRooms on_close={close} /> },
        Some(Modal::RenameRoom(id, current)) => html! {
            <dialogs::RenameRoom room_id={id} {current} on_close={close} />
        },
        Some(Modal::DeleteMessage(id, preview)) => html! {
            <dialogs::DeleteMessage
                message_id={id}
                {preview}
                on_close={close.clone()}
                on_deleted={{
                    let store = store.clone();
                    Callback::from(move |_: ()| {
                        store.dispatch(Action::CloseModal);
                        toast::neutral(&store, "Message deleted");
                    })
                }}
            />
        },
        Some(Modal::Confirm(c)) => html! {
            <ConfirmHost confirm={c} on_navigate={on_navigate.clone()} />
        },
        Some(Modal::Wallet) => html! { <dialogs::Wallet on_close={close} /> },
        Some(Modal::Shout) => html! { <shout::ShoutDialog on_close={close} /> },
        Some(Modal::ServerInfo) => html! { <dialogs::ServerInfoDialog on_close={close} /> },
        Some(Modal::More) => html! {
            <dialogs::MoreSheet
                route={route.clone()}
                on_close={close.clone()}
                // Every row both navigates and dismisses. Leaving the sheet up
                // over the screen it just opened would hide the thing it was
                // asked to reveal.
                on_navigate={{
                    let store = store.clone();
                    let on_navigate = on_navigate.clone();
                    Callback::from(move |r: Route| {
                        store.dispatch(Action::CloseModal);
                        on_navigate.emit(r);
                    })
                }}
            />
        },
        Some(Modal::Assistant(id)) => html! {
            <dialogs::Assistant room_id={id} on_close={close} />
        },
        Some(Modal::Files(id)) => html! {
            <dialogs::Files room_id={id} on_close={close} />
        },
    }
}

#[derive(Properties, PartialEq)]
struct ConfirmHostProps {
    confirm: crate::state::Confirm,
    on_navigate: Callback<Route>,
}

/// Runs a [`ConfirmAction`] and reports failure inside the dialog rather than
/// closing it — a confirmation that vanishes on error leaves the user with no
/// idea whether the thing happened.
#[function_component(ConfirmHost)]
fn confirm_host(p: &ConfirmHostProps) -> Html {
    let store = crate::state::use_store();
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);

    // One runner, two buttons: the destructive verb and — when the dialog has
    // one — its milder alternative. They differ only in which action they
    // carry, so they must not differ in how it is run, reported or recovered
    // from.
    let run = {
        let store = store.clone();
        let busy = busy.clone();
        let error = error.clone();
        let on_navigate = p.on_navigate.clone();
        Rc::new(move |action: ConfirmAction| {
            if *busy {
                return;
            }
            // Asking the second question is not work: it opens the follow-up
            // dialog in place of this one and nothing is sent, so there is no
            // request to be busy with and no error to recover from.
            if let ConfirmAction::ExitAsAdmin(id) = &action {
                store.dispatch(Action::OpenModal(Modal::Confirm(admin_exit_confirm(
                    &store, id,
                ))));
                return;
            }
            busy.set(true);
            error.set(None);
            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let action = action.clone();
            let on_navigate = on_navigate.clone();

            wasm_bindgen_futures::spawn_local(async move {
                let client = store.client.clone();
                // Read before the request: a room that has just been left,
                // hidden or deleted is gone from the store by the time the
                // refresh lands, and "Room hidden" without the name is a
                // confirmation you cannot check.
                let removed_name = match &action {
                    ConfirmAction::LeaveRoom(id)
                    | ConfirmAction::DeleteRoom(id)
                    | ConfirmAction::HideRoom(id) => store.room(id).map(|r| r.room.name.clone()),
                    _ => None,
                };
                let result: Result<(), String> = match &action {
                    // Handled before the spawn — listed so adding a case here
                    // later cannot silently fall through to "nothing happened".
                    ConfirmAction::ExitAsAdmin(_) => Ok(()),
                    ConfirmAction::LeaveRoom(id) => {
                        client.leave_room(id).await.map_err(|e| e.user_message())
                    }
                    ConfirmAction::DeleteRoom(id) => {
                        client.delete_room(id).await.map_err(|e| e.user_message())
                    }
                    ConfirmAction::HideRoom(id) => {
                        client.hide_room(id).await.map_err(|e| e.user_message())
                    }
                    ConfirmAction::DeleteAllMessages(id) => client
                        .delete_all_messages(id)
                        .await
                        .map_err(|e| e.user_message()),
                    ConfirmAction::KickMember(id, who) => client
                        .kick_member(id, who)
                        .await
                        .map_err(|e| e.user_message()),
                    ConfirmAction::BlockUser(who) => {
                        client.block_user(who).await.map_err(|e| e.user_message())
                    }
                    ConfirmAction::UnblockUser(who) => {
                        client.unblock_user(who).await.map_err(|e| e.user_message())
                    }
                    ConfirmAction::RemoveAdmin(id, who) => client
                        .remove_admin(id, who)
                        .await
                        .map_err(|e| e.user_message()),
                    ConfirmAction::EraseLocalData => {
                        crate::session::erase_local_data();
                        Ok(())
                    }
                    ConfirmAction::ForgetWallet => {
                        // `forget`, not `clear`: this one also turns the
                        // preference off, so the next sign-in does not write the
                        // phrase straight back.
                        crate::vault::forget();
                        Ok(())
                    }
                    ConfirmAction::SignOut => {
                        let _ = client.logout().await;
                        Ok(())
                    }
                };

                match result {
                    Ok(()) => {
                        store.dispatch(Action::CloseModal);
                        match &action {
                            // Never `window.location.reload()`: the router just
                            // navigates, so the socket and caches survive.
                            ConfirmAction::LeaveRoom(_)
                            | ConfirmAction::DeleteRoom(_)
                            | ConfirmAction::HideRoom(_) => {
                                // A room vanishing from the list is a large,
                                // silent change. Name what left, and — for the
                                // reversible one — where it went.
                                let name = removed_name.unwrap_or_default();
                                let lang = store.language;
                                match &action {
                                    ConfirmAction::HideRoom(_) => store.dispatch(Action::Toast(
                                        crate::state::ToastKind::Neutral,
                                        t(lang, Key::room_hidden_toast).replace("{name}", &name),
                                        Some(t(lang, Key::room_hidden_toast_body).into()),
                                    )),
                                    ConfirmAction::LeaveRoom(_) => toast::neutral(
                                        &store,
                                        t(lang, Key::room_left_toast).replace("{name}", &name),
                                    ),
                                    // Destroying reaches further than the room
                                    // list, so the toast says how far: the
                                    // pictures and attachments are off the
                                    // server's disk, not merely out of sight.
                                    _ => store.dispatch(Action::Toast(
                                        crate::state::ToastKind::Neutral,
                                        t(lang, Key::room_deleted_toast).replace("{name}", &name),
                                        Some(t(lang, Key::room_destroyed_toast_body).into()),
                                    )),
                                }
                                // Only removals count towards the swipe streak,
                                // and only once the server has agreed.
                                crate::components::room_list::settle_swipe_streak(&store);
                                actions::refresh_rooms(store.clone()).await;
                                on_navigate.emit(Route::Rooms);
                            }
                            ConfirmAction::EraseLocalData | ConfirmAction::SignOut => {
                                actions::sign_out(&store);
                                on_navigate.emit(Route::Login);
                            }
                            ConfirmAction::BlockUser(_) | ConfirmAction::UnblockUser(_) => {
                                actions::refresh_blocks(store.clone()).await;
                            }
                            // Nothing to refetch: forgetting the credential
                            // changes only what this browser has stored, and
                            // the session in memory keeps working until it is
                            // reloaded.
                            ConfirmAction::ForgetWallet => {
                                toast::success(&store, t(store.language, Key::phrase_forgotten));
                            }
                            _ => actions::refresh_rooms(store.clone()).await,
                        }
                    }
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        })
    };

    let on_confirm = {
        let run = run.clone();
        let action = p.confirm.action.clone();
        Callback::from(move |_: ()| run(action.clone()))
    };
    let alternative = p.confirm.alternative.clone();
    let on_alternative = alternative.as_ref().map(|alt| {
        let run = run.clone();
        let action = alt.action.clone();
        Callback::from(move |_: ()| run(action.clone()))
    });

    html! {
        <crate::components::modal::ConfirmDialog
            title={p.confirm.title.clone()}
            body={p.confirm.body.clone()}
            confirm_label={p.confirm.confirm_label.clone()}
            alternative_label={alternative.map(|a| a.label)}
            {on_alternative}
            busy={*busy}
            error={(*error).clone()}
            {on_confirm}
            on_cancel={{
                let store = store.clone();
                Callback::from(move |_: ()| store.dispatch(Action::CloseModal))
            }}
        />
    }
}

/// The second question an admin's exit asks.
///
/// Built here rather than at the menu because it depends on what the store
/// knows *now*: an admin with a colleague may walk away and leave the room
/// standing, and the last admin may not — the server refuses that leave, so
/// offering it would be a button that only ever produces an error. The sole
/// admin's dialog says why, and names the way out (promote somebody) rather
/// than leaving them to discover it.
fn admin_exit_confirm(store: &Store, id: &RoomId) -> crate::state::Confirm {
    let room = store.room(id);
    let name = room.map(|r| r.room.name.clone()).unwrap_or_default();
    // Absent from the store is treated as "sole admin": the cautious reading,
    // since it offers the option that cannot silently fail.
    let sole_admin = room.is_none_or(|r| r.admins.len() <= 1);
    admin_exit_dialog(store.language, &name, sole_admin, id)
}

/// The dialog itself, as a function of what was decided above.
///
/// Split from the store lookup so the two branches are testable without a
/// mounted app: which verb is primary, and whether "just leave" is offered at
/// all, is the whole behaviour worth pinning.
fn admin_exit_dialog(
    lang: crate::i18n::Lang,
    name: &str,
    sole_admin: bool,
    id: &RoomId,
) -> crate::state::Confirm {
    let confirm = crate::state::Confirm::new(
        t(lang, Key::destroy_room_title).replace("{name}", name),
        t(
            lang,
            if sole_admin {
                Key::destroy_room_sole_admin_body
            } else {
                Key::destroy_room_body
            },
        )
        .to_owned(),
        t(lang, Key::destroy_room).to_owned(),
        ConfirmAction::DeleteRoom(id.clone()),
    );
    if sole_admin {
        confirm
    } else {
        confirm.or(
            t(lang, Key::just_leave).to_owned(),
            ConfirmAction::LeaveRoom(id.clone()),
        )
    }
}

// --- browser plumbing ----------------------------------------------------

fn current_path() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .unwrap_or_else(|| "/".into())
}

fn push_state(route: &Route) {
    if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ =
            history.push_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&route.to_path()));
    }
}

fn set_title(title: &str) {
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        doc.set_title(title);
    }
}

/// Remove the pre-mount boot placeholder from `index.html`.
pub fn clear_boot_screen() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("app-loading"))
    {
        el.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use crate::state::ConfirmAlternative;

    fn room() -> RoomId {
        RoomId::new("room_1785295655035_a801a50e-9dc1-4af5-9954-597bc2831364").unwrap()
    }

    #[test]
    fn an_admin_with_a_colleague_is_offered_both_ways_out() {
        let c = admin_exit_dialog(Lang::En, "engineering", false, &room());

        // The destructive one is the primary — this dialog exists to ask it —
        // and leaving stays reachable without going back to the menu.
        assert_eq!(c.action, ConfirmAction::DeleteRoom(room()));
        assert!(c.title.contains("engineering"), "{}", c.title);
        assert_eq!(
            c.alternative,
            Some(ConfirmAlternative {
                label: "Just leave".into(),
                action: ConfirmAction::LeaveRoom(room()),
            })
        );
    }

    #[test]
    fn the_last_admin_is_not_offered_a_leave_the_server_would_refuse() {
        let c = admin_exit_dialog(Lang::En, "engineering", true, &room());

        assert_eq!(c.action, ConfirmAction::DeleteRoom(room()));
        assert_eq!(c.alternative, None, "the server refuses that leave");
        // And says what to do instead, rather than leaving them to find it.
        assert!(c.body.contains("admin"), "{}", c.body);
    }

    #[test]
    fn both_bodies_say_the_files_go_too() {
        // The dialog is the only place a user is told that destroying reaches
        // the disk. If that sentence goes missing, the consent goes with it.
        for sole in [true, false] {
            let body = admin_exit_dialog(Lang::En, "engineering", sole, &room()).body;
            assert!(body.contains("attachments"), "{body}");
            assert!(body.contains("pictures"), "{body}");
            assert!(body.contains("disk"), "{body}");
        }
    }

    #[test]
    fn the_question_is_asked_in_the_readers_language() {
        // Not a translation check — the completeness of the table is tested in
        // `i18n` — but the dialog must actually go through it rather than
        // hard-coding the English copy for the one string it composes.
        let c = admin_exit_dialog(Lang::Ko, "engineering", true, &room());
        assert!(c.title.contains("engineering"), "{}", c.title);
        assert_ne!(
            c.confirm_label,
            admin_exit_dialog(Lang::En, "engineering", true, &room()).confirm_label
        );
    }
}
