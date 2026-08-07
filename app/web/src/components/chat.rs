//! Screen 3 — Chat view (DESIGN.md §7). The product; everything else exists to
//! get here.
//!
//! Two states the reference client fails silently on are made visible here, and
//! they are the reason this file is as long as it is:
//!
//! * **Key rotation pending.** A banner explains it, the composer locks, and a
//!   "Rotate now" button performs the re-key. Silently swallowing sends in an
//!   E2EE product is not acceptable.
//! * **Offline.** The connection pill and a banner say so, the composer stays
//!   enabled, and sends queue as pending bubbles that flush oldest-first on
//!   reconnect.

use pocketskynet_core::{MessageId, RoomId};
use std::collections::HashMap;
use std::rc::Rc;
use web_sys::HtmlElement;

use yew::prelude::*;

use crate::actions;
use crate::api::Message;
use crate::crypto::{decrypt_message, Decrypted};
use crate::format;
use crate::route::Route;
use crate::state::{use_store, Action, Confirm, ConfirmAction, Load, Modal, PostBlock};
use crate::store::{starts_new_day, starts_new_group};

use super::common::{
    Back, Badge, Banner, BusyButton, ConnPill, Empty, Ident, IdentSize, Lock, Popover,
    PresenceLabel, Spinner,
};
use super::composer::{Composer, Picker};
use super::icons;
use super::message::{DayMark, MessageRow};
use super::toast;
use crate::i18n::{t, Key, Lang};

#[derive(Properties, PartialEq)]
pub struct ChatProps {
    pub room_id: RoomId,
    pub on_navigate: Callback<Route>,
    pub on_refresh: Callback<RoomId>,
    pub on_typing: Callback<RoomId>,
}

#[function_component(Chat)]
pub fn chat(p: &ChatProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let menu_open = use_state(|| false);
    let rotating = use_state(|| false);
    let rotate_error = use_state(|| Option::<String>::None);
    let loading_older = use_state(|| false);
    let picker_for = use_state(|| Option::<Option<MessageId>>::None);
    // The message the picker is reacting to, flattened out of the two-level
    // Option: outer = "picker is open", inner = "a specific message, or the
    // composer". Captured here because `html!` cannot hold statements.
    let picker_target = (*picker_for).clone().flatten();
    let stream_ref = use_node_ref();

    let room = store.room(&p.room_id).cloned();
    let me = store.me().cloned();
    let state = store.room_state(&p.room_id).cloned().unwrap_or_default();
    let load = store.room_load.get(&p.room_id).cloned().unwrap_or_default();

    // How far from the bottom still counts as "reading the newest".
    //
    // Generous on purpose. A message can be two hundred pixels tall, and a
    // threshold tight enough to mean "exactly at the bottom" would unpin
    // somebody who is plainly still there — a stray trackpad nudge, or the
    // browser's own rounding after an image finishes loading and reflows the
    // stream under them.
    const PINNED_SLACK_PX: f64 = 120.0;

    /// The same slack, scaled to the viewport.
    ///
    /// A flat 120px is a quarter of a laptop's message pane and a *sliver* of
    /// a phone's, where one bubble can be taller than the whole allowance —
    /// so a thumb-scroll that lands one message short of the end reads as
    /// "gone off to read history" and the stream stops following. Taking a
    /// fifth of the pane keeps the meaning ("still looking at the newest")
    /// the same on both.
    fn pin_slack(client_height: f64) -> f64 {
        PINNED_SLACK_PX.max(client_height * 0.2)
    }

    /// How long after a gesture a scroll still counts as the reader's doing.
    ///
    /// Generous enough to cover the tail of a fling on a touchscreen, which
    /// keeps firing `scroll` long after the finger has gone, and short enough
    /// that a reflow a second later is not mistaken for it.
    const GESTURE_WINDOW_MS: f64 = 700.0;

    fn now_ms_f64() -> f64 {
        #[cfg(target_arch = "wasm32")]
        {
            js_sys::Date::now()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            0.0
        }
    }

    // Whether the stream was at the bottom *before* this render's new message
    // landed. Read during render rather than inside the effect: by the time
    // the effect runs the DOM already contains the new row, so the scroll
    // position it would measure is the answer to a different question.
    // Which threads are expanded, and which message the composer replies into.
    // Declared here rather than beside the rendering because the settle below
    // has to know: a thread's replies are visible only while it is open, so
    // "how much is on screen" cannot be answered without this.
    let open_threads = use_state(std::collections::HashSet::<MessageId>::new);
    let reply_to = use_state(|| Option::<MessageId>::None);

    // Whether the room has loaded, and therefore whether the stream element
    // exists in the DOM. The listeners below key on this.
    let room_ready = room.is_some();

    // Decrypt once per message, not once per render.
    //
    // Every row's body used to be decrypted inline in the render loop, so a
    // room of twenty encrypted messages ran twenty AES+HMAC passes on every
    // single render — and a render happens on every keystroke in the composer,
    // every typing indicator, every reaction. That is the cost behind the
    // flicker: the main thread is busy re-deriving text it already had, while
    // the rows it is rebuilding have no height yet.
    //
    // Keyed by message id, holding the `msg_serial` it was decrypted at. An
    // edit advances the serial, which is exactly the signal that the cached
    // plaintext is stale — so a message is re-decrypted when, and only when,
    // its content actually changed.
    let plaintext = use_mut_ref(HashMap::<MessageId, (i64, Decrypted)>::new);
    // What the cache's entries were computed under: the room, and the
    // bundle's epoch coverage. Compared and cleared *during* render, below,
    // once `bundle` is in hand — an effect is one frame too late. Effects run
    // after the render commits, and clearing a RefCell schedules nothing, so
    // the render that first saw the new key would have served every row from
    // the stale cache and the sealed bubbles would sit there until something
    // unrelated re-rendered.
    let plaintext_stamp = use_mut_ref(|| Option::<(RoomId, Option<(usize, Option<i64>)>)>::None);

    let was_pinned = use_mut_ref(|| true);
    // When the reader last touched the scroll themselves.
    //
    // The stream fires `scroll` for two very different reasons, and treating
    // them alike is what broke this: a finger or a wheel, and the browser
    // clamping `scrollTop` because the content under it changed size. Rows do
    // change size — an attachment row collapses to nothing while it
    // re-renders and springs back a moment later — and the clamp that follows
    // looks exactly like somebody scrolling up. So the view was marked
    // "reading history", the correction refused to run, and the message just
    // sent stayed off-screen. Only a real gesture may unpin.
    let last_gesture = use_mut_ref(|| 0.0f64);
    // A count of what arrived while scrolled away, for the jump-down pill.
    let unseen = use_state(|| 0u32);
    // Whether this room's backlog has already been settled once.
    //
    // The room-change effect below cannot do this job on its own: it fires when
    // the id changes, which is *before* the messages arrive, so it has nothing
    // to scroll. The first real settle therefore happens in the count effect —
    // and without this flag it would animate the entire backlog, smoothly
    // dragging three thousand pixels of history past the reader on every room
    // open. That is the smear the animation is supposed to avoid, not produce.
    let settled = use_mut_ref(|| false);
    let last_pending = use_mut_ref(|| 0usize);
    // Your own outbound messages, which live in `store.pending` until the
    // server acknowledges them — *not* in `state.messages`. Keying the settle
    // on the message count alone therefore missed the single most obvious
    // case there is: you press Enter, your bubble appears below the fold, and
    // the view does not move. It is also the one case where the reader's
    // scroll position should be overridden rather than respected — somebody
    // who scrolled up and then typed wants to see what they just said.
    let pending_count = store.pending.get(&p.room_id).map(|q| q.len()).unwrap_or(0);
    // What the channel actually shows: top-level rows, nothing else.
    //
    // **Not** `state.messages.len()`, which counts thread replies the channel
    // does not display — a reply to a collapsed thread bumped that count,
    // scrolled the stream, and put "1 new message" on the pill for something
    // nowhere on screen. Pressing it took you to the bottom to find nothing.
    //
    // And **not** a sum over each open thread's replies either, which was the
    // first attempt: quadratic in a busy room, and it made expanding a thread
    // look identical to a message arriving, so opening one scrolled away from
    // the very thread just opened. Counting only what the channel lists means
    // expanding a thread does not move the count at all — the effect simply
    // does not fire, which is exactly the wanted behaviour and needs no
    // special case to express.
    let visible_count = state.ordered_top_level(&store.blocks).len();

    {
        let stream_ref = stream_ref.clone();
        let was_pinned = was_pinned.clone();
        let unseen = unseen.clone();
        let settled = settled.clone();
        let last_pending = last_pending.clone();
        // Both counts drive the settle: one for messages arriving, one for
        // messages leaving.
        let deps = (visible_count, pending_count);
        // Anchoring on the message count rather than a revision: edits and
        // reactions must not move the scroll, and history loads prepend —
        // both of those change the revision without adding a row at the
        // bottom, and settling the scroll for either yanks the reader.
        use_effect_with(deps, move |(count, pending_count)| {
            let (count, pending_count) = (*count, *pending_count);
            let Some(el) = stream_ref.cast::<HtmlElement>() else {
                return;
            };

            // An empty stream is not a settle. This effect also runs on mount,
            // when the room's messages have not arrived yet — and letting that
            // run consume the "first settle" below meant the real backlog got
            // the *smooth* path, animating three thousand pixels of history on
            // every room open. Worse, a second load batch landing during that
            // animation was counted as "unseen" and raised the jump pill on a
            // reader who had not scrolled anywhere.
            if count == 0 && pending_count == 0 {
                return;
            }

            // The first settle after opening a room is a jump, not a
            // journey: the backlog was already there when the reader arrived,
            // so there is no motion to explain.
            let first = !*settled.borrow();
            *settled.borrow_mut() = true;

            // A message you sent always brings you with it. Anything else
            // respects where you are reading.
            let sent_by_me = pending_count > *last_pending.borrow();
            *last_pending.borrow_mut() = pending_count;
            if sent_by_me {
                *was_pinned.borrow_mut() = true;
            }

            let pinned = *was_pinned.borrow();
            if first {
                scroll_to_latest(&el, false);
                unseen.set(0);
            } else if pinned {
                scroll_to_latest(&el, true);
                unseen.set(0);
            } else {
                // Left where they are, deliberately. Dragging somebody out of
                // the history they are reading is worse than a delayed read —
                // the pill below is how they come back, on their terms.
                unseen.set(*unseen + 1);
            }
        });
    }

    // Recompute the pin on every scroll, so the *next* arrival knows whether
    // this person is still at the bottom.
    let on_stream_scroll = {
        let was_pinned = was_pinned.clone();
        let unseen = unseen.clone();
        let last_gesture = last_gesture.clone();
        Callback::from(move |e: Event| {
            let Some(el) = e.target_dyn_into::<HtmlElement>() else {
                return;
            };
            let distance =
                el.scroll_height() as f64 - el.scroll_top() as f64 - el.client_height() as f64;
            if distance <= pin_slack(el.client_height() as f64) {
                // Arriving at the bottom always means "following", however
                // you got here.
                *was_pinned.borrow_mut() = true;
            } else if now_ms_f64() - *last_gesture.borrow() < GESTURE_WINDOW_MS {
                // Away from the bottom, and the reader put it there.
                *was_pinned.borrow_mut() = false;
            }
            // Otherwise: away from the bottom with no gesture behind it — a
            // reflow moved the content, not the reader. Leave the intent
            // alone; the observer below puts the view back.
            // Scrolling back down by hand clears the pill without a press.
            if *was_pinned.borrow() && *unseen > 0 {
                unseen.set(0);
            }
        })
    };

    let jump_to_latest = {
        let stream_ref = stream_ref.clone();
        let was_pinned = was_pinned.clone();
        let unseen = unseen.clone();
        Callback::from(move |_: MouseEvent| {
            if let Some(el) = stream_ref.cast::<HtmlElement>() {
                scroll_to_latest(&el, true);
            }
            *was_pinned.borrow_mut() = true;
            unseen.set(0);
        })
    };

    // Expanding a thread moves the goalposts, so re-measure rather than trust.
    //
    // `was_pinned` is only ever updated by scroll events, and revealing a
    // thread's replies fires none — it just inserts several hundred pixels.
    // The flag therefore stayed `true` while the reader was left far from the
    // bottom, and the media listener below then did the damage: the replies
    // just revealed load their avatars, each `load` finds "pinned", and the
    // view is flung to the end of the room — away from the very thread that
    // was opened to be read. Measuring after the render is the honest answer
    // to "are they still following?".
    {
        let stream_ref = stream_ref.clone();
        let was_pinned = was_pinned.clone();
        let open = (*open_threads).clone();
        use_effect_with(open, move |_| {
            if let Some(el) = stream_ref.cast::<HtmlElement>() {
                let distance =
                    el.scroll_height() as f64 - el.scroll_top() as f64 - el.client_height() as f64;
                *was_pinned.borrow_mut() = distance <= pin_slack(el.client_height() as f64);
            }
        });
    }

    // Media that changes a row's height *after* it was scrolled past.
    //
    // This is the bug the flat "scroll on new message" never survived, and it
    // is invisible in a room of plain text. A message carrying a video or an
    // image renders at almost no height — no poster, no intrinsic dimensions —
    // and the settle below runs against that. Then the poster decodes, the
    // metadata arrives, and the row grows by several hundred pixels. The
    // stream is now nowhere near the bottom, and **no scroll event fires for
    // content growth**, so nothing here would ever notice. Rows are variable
    // height; treating one settle as final was the mistake.
    //
    // `load` and `loadedmetadata` do not bubble, so this listens in the
    // capture phase on the container — one listener for every image, video and
    // avatar in the room, present and future, instead of a subscription per
    // row that would have to be managed as messages arrive.
    {
        let stream_ref = stream_ref.clone();
        let was_pinned = was_pinned.clone();
        let last_gesture = last_gesture.clone();
        // Keyed on the room *and on whether it has loaded* — not on `()`.
        //
        // This component returns a spinner while the room is still being
        // fetched, so on the first render there is no `.fn-stream` in the DOM
        // at all. An effect with `()` deps runs exactly then, finds nothing to
        // attach to, and — having no dependency that can change — never tries
        // again. The listener was therefore missing for the whole session
        // whenever a room was opened cold: by URL, by reload, or on any
        // connection slow enough that the room list had not arrived yet. Which
        // is exactly when a room full of video needs it.
        use_effect_with((p.room_id.clone(), room_ready), move |_| {
            let listeners = stream_ref.cast::<HtmlElement>().map(|el| {
                let options = gloo_events::EventListenerOptions::enable_prevent_default();
                let capture = gloo_events::EventListenerOptions {
                    phase: gloo_events::EventListenerPhase::Capture,
                    ..options
                };
                // A wheel, a finger, a scrolling key, or the scrollbar itself.
                // These are the only things that may *unpin* — see
                // `last_gesture`. `pointerdown` is not padding: dragging the
                // scrollbar produces scroll events and no wheel or touch at
                // all, so without it a mouse user who scrolled up to read
                // would be dragged back down by the next message.
                let gestures =
                    ["wheel", "touchstart", "touchmove", "keydown", "pointerdown"].map(|event| {
                        let last_gesture = last_gesture.clone();
                        gloo_events::EventListener::new_with_options(
                            &el,
                            event,
                            capture,
                            move |_| {
                                *last_gesture.borrow_mut() = now_ms_f64();
                            },
                        )
                    });

                let media = ["load", "loadedmetadata"].map(|event| {
                    let stream_ref = stream_ref.clone();
                    let was_pinned = was_pinned.clone();
                    gloo_events::EventListener::new_with_options(&el, event, capture, move |_| {
                        // Only while following. Somebody reading history
                        // must not be yanked because a video three
                        // screens below them finished decoding.
                        if !*was_pinned.borrow() {
                            return;
                        }
                        if let Some(el) = stream_ref.cast::<HtmlElement>() {
                            // Instant: this is a correction to a settle
                            // that already happened, not a new arrival.
                            // Animating it would read as drift.
                            scroll_to_latest(&el, false);
                        }
                    })
                });

                // The stream re-rendering, which `load` cannot stand in for.
                //
                // Attachment rows collapse to nothing and spring back as they
                // re-render, and with a cached image no `load` fires at all —
                // so the media listener above had nothing to react to and the
                // view stayed stranded wherever the collapse left it. A
                // mutation is the one signal that is always there. Re-settling
                // when already at the bottom is a no-op, so this is cheap
                // however chatty the room.
                let observer = stream_ref.cast::<HtmlElement>().and_then(|el| {
                    use wasm_bindgen::JsCast;
                    let target = el.clone();
                    let was_pinned = was_pinned.clone();
                    let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                        if !*was_pinned.borrow() {
                            return;
                        }
                        scroll_to_latest(&target, false);
                    });
                    let observer =
                        web_sys::MutationObserver::new(callback.as_ref().unchecked_ref()).ok()?;
                    let init = web_sys::MutationObserverInit::new();
                    init.set_child_list(true);
                    init.set_subtree(true);
                    observer.observe_with_options(&el, &init).ok()?;
                    Some((observer, callback))
                });

                (gestures, media, observer)
            });
            move || {
                if let Some((_, _, Some((observer, _)))) = &listeners {
                    observer.disconnect();
                }
                drop(listeners)
            }
        });
    }

    // The on-screen keyboard.
    //
    // Focusing the composer on a phone takes roughly half the screen away, and
    // the browser shrinks the stream rather than scrolling it — so the message
    // you were looking at, including the one you just sent, ends up behind the
    // keyboard. No scroll event fires for a resize, so nothing else here would
    // notice. Re-settling on resize is what keeps "the newest message is at
    // the bottom" true when the bottom moves.
    //
    // Instant, not smooth: this rides a layout change the user caused, and
    // animating it would race the keyboard sliding up.
    {
        let stream_ref = stream_ref.clone();
        let was_pinned = was_pinned.clone();
        // Same keying, for the same reason: the element has to exist first.
        use_effect_with((p.room_id.clone(), room_ready), move |_| {
            let listener = web_sys::window().map(|window| {
                gloo_events::EventListener::new(&window, "resize", move |_| {
                    if !*was_pinned.borrow() {
                        return;
                    }
                    if let Some(el) = stream_ref.cast::<HtmlElement>() {
                        scroll_to_latest(&el, false);
                    }
                })
            });
            move || drop(listener)
        });
    }

    // Opening a room is not an arrival: the backlog is already below the fold
    // and animating a thousand pixels of it is a smear, not a transition. So
    // the first settle after a room change is instant, and `was_pinned` is
    // reset because the previous room's scroll position says nothing here.
    {
        let stream_ref = stream_ref.clone();
        let was_pinned = was_pinned.clone();
        let unseen = unseen.clone();
        let settled = settled.clone();
        use_effect_with(p.room_id.clone(), move |_| {
            *was_pinned.borrow_mut() = true;
            // Arm the instant first settle for the room being opened. The
            // messages are usually not here yet — the count effect above is
            // what actually performs it, once they are.
            *settled.borrow_mut() = false;
            unseen.set(0);
            if let Some(el) = stream_ref.cast::<HtmlElement>() {
                scroll_to_latest(&el, false);
            }
        });
    }

    let Some(room) = room else {
        return match load {
            Load::Error(e) => html! {
                <Empty art="⚠️" title={t(lang, Key::room_unavailable)} art_class="fn-art--offline"
                       description={e} is_error=true>
                    { back_to_rooms(lang, &p.on_navigate) }
                </Empty>
            },
            _ => html! {
                <div class="fn-chat">
                    <Spinner large=true label={t(lang, Key::opening_room)} />
                    <p class="fn-muted">{ t(lang, Key::opening_room) }</p>
                </div>
            },
        };
    };

    let Some(me) = me else {
        return html! {};
    };

    // Which threads are expanded, and which message the composer is replying
    // into. Component state rather than store state: both are about *this
    // screen right now*, and a thread left open in a room you navigated away
    // from is not something to restore.

    let toggle_thread = {
        let open_threads = open_threads.clone();
        Callback::from(move |id: MessageId| {
            let mut next = (*open_threads).clone();
            if !next.remove(&id) {
                next.insert(id);
            }
            open_threads.set(next);
        })
    };
    let start_reply = {
        let reply_to = reply_to.clone();
        let open_threads = open_threads.clone();
        Callback::from(move |id: MessageId| {
            // Opening the thread as well: replying into something you cannot
            // see is how people answer the wrong message.
            let mut next = (*open_threads).clone();
            next.insert(id.clone());
            open_threads.set(next);
            reply_to.set(Some(id));
        })
    };
    let cancel_reply = {
        let reply_to = reply_to.clone();
        Callback::from(move |_: MouseEvent| reply_to.set(None))
    };

    let on_knowledge = {
        let store = store.clone();
        let on_navigate = p.on_navigate.clone();
        Callback::from(move |seed: crate::state::KnowledgeSeed| {
            store.dispatch(Action::SeedKnowledge(seed));
            on_navigate.emit(Route::Knowledge);
        })
    };

    // One shared list for every row: the names highlighting looks for, and the
    // handles that mean "you". Built once per render rather than per message —
    // a hundred rows cloning a hundred-member roster is the most expensive
    // thing a busy room could do to draw a chip.
    let mention_names: Rc<Vec<String>> = Rc::new(
        room.members
            .iter()
            .map(|m| m.user.display_name())
            .chain(room.members.iter().map(|m| m.user_address.to_string()))
            .collect(),
    );
    let my_handles: Rc<Vec<String>> = Rc::new(
        room.members
            .iter()
            .find(|m| m.user_address == me)
            .map(|m| vec![m.user.display_name(), m.user_address.to_string()])
            .unwrap_or_else(|| vec![me.to_string()]),
    );

    let is_admin = room.is_admin(&me);
    let rotation_pending = room.room.key_rotation_pending;
    // What to call this conversation. A channel has a name somebody chose; a
    // DM does not, and the server's placeholder must never reach the screen.
    // See `RoomWithMembers::title_for` — the answer differs per viewer, so it
    // can only be worked out here.
    let title = room.title_for(&me);
    // In a one-to-one DM the header stands for a person, so it carries their
    // status; a channel header stands for a room, which is not somewhere
    // anybody is. This is the single most useful place for it — it is what
    // tells you whether the message you are about to type will be read now or
    // tomorrow morning.
    let peer_presence = match room.others(&me).as_slice() {
        [one] if room.is_direct() => store.presence_of(&one.wallet_address),
        _ => pocketskynet_core::PresenceStatus::Offline,
    };
    // Carries *why* an encrypted post cannot succeed, not merely that it
    // cannot: the composer's placeholder is the only explanation the user gets,
    // and the remedies differ.
    let composer_blocked = store.post_block(&p.room_id);
    let offline = !store.online;
    let bundle = store.bundle(&p.room_id).cloned();

    // Invalidate the plaintext cache the moment its inputs change, so this
    // very render decrypts fresh. Keyed on the bundle's *coverage*, not the
    // room's `current_key_version`: after a rotation that happened while this
    // device was away, the room names the new epoch before the key for it
    // arrives, and the version alone would never notice the bundle catching
    // up — every row of that epoch would stay cached as sealed.
    {
        let stamp = (p.room_id.clone(), bundle.as_ref().map(|b| b.coverage()));
        let mut last = plaintext_stamp.borrow_mut();
        if last.as_ref() != Some(&stamp) {
            plaintext.borrow_mut().clear();
            *last = Some(stamp);
        }
    }
    let now = format::now_ms();
    let tz = format::tz_offset_minutes();

    // --- callbacks -------------------------------------------------------

    let on_send = {
        let store = store.clone();
        let room_id = p.room_id.clone();
        let reply_to = reply_to.clone();
        let roster = room.members.clone();
        let me_for_send = me.clone();
        Callback::from(move |text: String| {
            let now = format::now_ms();
            let local_id = crate::state::next_local_id();
            // Resolved here, from the text as typed and this room's roster.
            // The server parses plaintext too, but it cannot parse ciphertext
            // and cannot recover a name with a space in it — see
            // `crate::mentions`.
            let extras = actions::SendExtras {
                parent: (*reply_to).clone(),
                mentions: crate::mentions::resolve(&text, &roster, &me_for_send),
            };
            store.dispatch(Action::QueueSend(
                room_id.clone(),
                local_id,
                text.clone(),
                now,
            ));
            // The reply target is consumed by the send: the next message goes
            // to the channel unless it is aimed again. A sticky thread is how
            // people post an unrelated remark into somebody's conversation.
            reply_to.set(None);
            let store2 = store.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                actions::send_message(store2, room_id, local_id, text, extras).await;
            });
        })
    };

    let on_react = {
        let store = store.clone();
        Callback::from(move |(id, code, mine): (MessageId, String, bool)| {
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let client = store.client.clone();
                let result = if mine {
                    client.remove_emoticon(&id, &code).await
                } else {
                    let result = client.add_emoticon(&id, &code).await;
                    if result.is_ok() {
                        // Adding only — paying for the toggle would make
                        // tapping the same emoji on and off a load farm.
                        crate::progression::award(pocketskynet_core::progression::Award::Reaction);
                    }
                    result
                };
                if let Err(e) = result {
                    toast::error(
                        &store,
                        "Couldn't update the reaction",
                        Some(e.user_message()),
                    );
                }
            });
        })
    };

    let on_copy = {
        let store = store.clone();
        Callback::from(move |text: String| {
            if text.is_empty() {
                return;
            }
            let store = store.clone();
            super::common::copy_then(&text, move |ok| {
                if ok {
                    toast::success(&store, t(lang, Key::copied));
                } else {
                    toast::error(
                        &store,
                        t(lang, Key::couldnt_copy),
                        Some(t(lang, Key::clipboard_blocked).into()),
                    );
                }
            });
        })
    };

    let on_delete = {
        let store = store.clone();
        Callback::from(move |(id, preview): (MessageId, Option<String>)| {
            store.dispatch(Action::OpenModal(Modal::DeleteMessage(id, preview)));
        })
    };

    let on_edit = {
        let store = store.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |(id, text): (MessageId, String)| {
            let store = store.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                // An edit is a new event under the *current* epoch with a fresh
                // IV. Omitting iv/hmac would silently downgrade the row to
                // plaintext server-side (API.md §6.10.3).
                let encrypted = store.room(&room_id).is_some_and(|r| r.has_encryption);
                let body = if encrypted {
                    let Some((version, key)) = store
                        .bundle(&room_id)
                        .and_then(|b| b.latest().map(|(v, k)| (v, *k)))
                    else {
                        toast::error(
                            &store,
                            t(lang, Key::cant_edit),
                            Some("No room key on this device.".into()),
                        );
                        return;
                    };
                    match crate::crypto::encrypted_body(&key, version, &room_id, &text) {
                        Ok(b) => b,
                        Err(e) => {
                            toast::error(&store, t(lang, Key::cant_edit), Some(e.to_string()));
                            return;
                        }
                    }
                } else {
                    crate::crypto::plaintext_body(&text)
                };
                // Re-declared from the edited text, not carried over: an edit
                // that removes a picture has to stop claiming the room shows
                // it, or the bytes would outlive every message naming them.
                let body = body.showing(crate::media::hosted_names(&text));
                match store.client.edit_message(&id, &body).await {
                    Ok(m) => store.dispatch(Action::Sync(room_id, vec![m])),
                    Err(e) => toast::error(
                        &store,
                        t(lang, Key::couldnt_save_edit),
                        Some(e.user_message()),
                    ),
                }
            });
        })
    };

    let on_rotate = {
        let store = store.clone();
        let room_id = p.room_id.clone();
        let rotating = rotating.clone();
        let rotate_error = rotate_error.clone();
        Callback::from(move |_: MouseEvent| {
            if *rotating {
                return;
            }
            rotating.set(true);
            rotate_error.set(None);
            let store = store.clone();
            let room_id = room_id.clone();
            let rotating = rotating.clone();
            let rotate_error = rotate_error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match actions::rotate_key(store.clone(), room_id).await {
                    Ok(()) => toast::success(&store, t(lang, Key::room_key_rotated)),
                    Err(e) => rotate_error.set(Some(e)),
                }
                rotating.set(false);
            });
        })
    };

    let on_load_older = {
        let store = store.clone();
        let room_id = p.room_id.clone();
        let loading_older = loading_older.clone();
        Callback::from(move |_: MouseEvent| {
            if *loading_older {
                return;
            }
            loading_older.set(true);
            let store = store.clone();
            let room_id = room_id.clone();
            let loading_older = loading_older.clone();
            wasm_bindgen_futures::spawn_local(async move {
                actions::load_older(store, room_id).await;
                loading_older.set(false);
            });
        })
    };

    // --- render ----------------------------------------------------------

    // The channel shows top-level messages only. Replies are in `state` —
    // `/sync` delivered them — and are rendered under their parent when its
    // thread is open, which is what makes opening one instant and offline.
    let visible: Vec<&Message> = state.ordered_top_level(&store.blocks);
    let pending = store.pending.get(&p.room_id).cloned().unwrap_or_default();
    let typists: Vec<String> = store
        .typing
        .typists(&p.room_id, &me)
        .iter()
        .map(|w| {
            room.members
                .iter()
                .find(|m| &m.user_address == w)
                .map(|m| m.user.display_name())
                .unwrap_or_else(|| w.abbreviated())
        })
        .collect();

    // Every child of `.fn-stream` is built into a single sequence in which
    // *every* entry carries a key, because Yew flattens `{ for … }` into the
    // parent's child list rather than nesting it. The children of this element
    // were once four expressions — two `if`s and two `for`s — and Yew saw one
    // flat list of thirty, of which the first two (the `if`s, empty lists when
    // their condition was false) had no key. One unkeyed sibling makes
    // `fully_keyed` false for the whole list, so all twenty-eight rows were
    // matched *by position, counting from the end*. Appending a message shifted
    // every position by one and Yew tore down and rebuilt every row: images
    // refetched, ciphertext re-decrypted, the list visibly flashed, and the
    // scroll position was clamped so the newest message landed off-screen.
    //
    // Keying the rows alone did not fix it, and could not: the keys were never
    // consulted. The conditionals have to be *in* the sequence, or absent from
    // it — hence a `Vec` rather than markup.
    let mut rows: Vec<Html> = Vec::with_capacity(visible.len() + pending.len() + 2);
    if state.has_more_history && !visible.is_empty() {
        rows.push(html! {
            <button
                type="button"
                key="load-earlier"
                class="topcoat-button--quiet"
                disabled={*loading_older}
                onclick={on_load_older.clone()}
            >
                { if *loading_older { t(lang, Key::loading) } else { t(lang, Key::load_earlier) } }
            </button>
        });
    }
    if visible.is_empty() && pending.is_empty() {
        rows.push(html! {
            <div class="fn-stream__item" key="empty">
                { empty_stream(lang, &load, room.has_encryption) }
            </div>
        });
    }

    rows.extend(visible.iter().enumerate().map(|(i, m)| {
        let prev: Option<&Message> = (i > 0).then(|| visible[i - 1]);
        let body = decrypt_cached(&plaintext, &bundle, &p.room_id, m);
        html! {
            // Keyed on the **outermost** node of each list item.
            //
            // This used to be a bare `<>` fragment with the key on
            // the `MessageRow` inside it. Yew matches list items by
            // the key on the node it iterates, so a fragment whose
            // key is one level down is an *unkeyed* list: every
            // render tore down all eighteen rows and built them
            // again. That is the flicker — images refetch,
            // ciphertext re-decrypts, every row collapses to
            // nothing and springs back — and it is also what
            // clamped the scroll position and stranded the newest
            // message off-screen.
            //
            // `display: contents` on the wrapper, so the rows stay
            // direct flex children of the stream and the gap
            // between them is unchanged. The element exists only
            // to carry the key.
            <div class="fn-stream__item" key={m.id.to_string()}>
                if starts_new_day(prev, m, tz) {
                    <DayMark timestamp={m.message_timestamp} {now} {tz} />
                }
                <MessageRow
                    key={m.id.to_string()}
                    message={(*m).clone()}
                    {body}
                    is_own={m.sender_address == me}
                    grouped={!starts_new_group(prev, m, tz)}
                    room_encrypted={room.has_encryption}
                    reactions={state.reactions_for(&m.id, &store.blocks)}
                    me={me.clone()}
                    chain={store.chain.clone()}
                    {tz}
                    on_react={on_react.clone()}
                    on_copy={on_copy.clone()}
                    on_delete={on_delete.clone()}
                    on_edit={on_edit.clone()}
                    on_open_picker={{
                        let picker_for = picker_for.clone();
                        Callback::from(move |id: MessageId| picker_for.set(Some(Some(id))))
                    }}
                    picker_open={picker_target.as_ref() == Some(&m.id)}
                    on_close_picker={{
                        let picker_for = picker_for.clone();
                        Callback::from(move |_: ()| picker_for.set(None))
                    }}
                    on_knowledge={on_knowledge.clone()}
                    mention_names={mention_names.clone()}
                    my_handles={my_handles.clone()}
                    reply_count={state.reply_count(m, &store.blocks)}
                    thread_open={open_threads.contains(&m.id)}
                    on_toggle_thread={toggle_thread.clone()}
                    on_reply={start_reply.clone()}
                />
                if open_threads.contains(&m.id) {
                    <div class="fn-thread" role="group"
                         aria-label={t(lang, Key::thread)}>
                        { for state.replies_to(&m.id, &store.blocks).into_iter().map(|r| {
                            let body =
                                decrypt_cached(&plaintext, &bundle, &p.room_id, r);
                            html! {
                                <MessageRow
                                    key={r.id.to_string()}
                                    message={r.clone()}
                                    {body}
                                    is_own={r.sender_address == me}
                                    grouped=false
                                    room_encrypted={room.has_encryption}
                                    reactions={state.reactions_for(&r.id, &store.blocks)}
                                    me={me.clone()}
                                    chain={store.chain.clone()}
                                    {tz}
                                    on_react={on_react.clone()}
                                    on_copy={on_copy.clone()}
                                    on_delete={on_delete.clone()}
                                    on_edit={on_edit.clone()}
                                    on_open_picker={{
                                        let picker_for = picker_for.clone();
                                        Callback::from(move |id: MessageId| {
                                            picker_for.set(Some(Some(id)))
                                        })
                                    }}
                                    picker_open={picker_target.as_ref() == Some(&r.id)}
                                    on_close_picker={{
                                        let picker_for = picker_for.clone();
                                        Callback::from(move |_: ()| picker_for.set(None))
                                    }}
                                    on_knowledge={on_knowledge.clone()}
                                    mention_names={mention_names.clone()}
                                    my_handles={my_handles.clone()}
                                    in_thread=true
                                />
                            }
                        }) }
                        // Always offered, even on a thread that
                        // already has replies: the alternative is
                        // scrolling back to the parent to find the
                        // one control that continues the thread
                        // you are looking at.
                        //
                        // Keyed for the same reason every row is:
                        // it is a sibling of the replies inside
                        // this list, and an unkeyed sibling turns
                        // keyed matching off for all of them.
                        <button
                            type="button"
                            key="reply-in-thread"
                            class="fn-thread__reply"
                            onclick={{
                                let start_reply = start_reply.clone();
                                let id = m.id.clone();
                                Callback::from(move |_: MouseEvent| {
                                    start_reply.emit(id.clone())
                                })
                            }}
                        >{ t(lang, Key::reply_in_thread) }</button>
                    </div>
                }
            </div>
        }
    }));
    rows.extend(pending.iter().map(|q| {
        html! {
                        <div class="fn-stream__item" key={format!("pending-{}", q.local_id)}>
                            { pending_bubble(lang, q, &store, &p.room_id) }
                        </div>
        }
    }));

    html! {
        <div class="fn-chat">
            <header class="fn-chat__head">
                <Back onclick={{
                    let on_navigate = p.on_navigate.clone();
                    Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Rooms))
                }} />
                <Ident
                    seed={p.room_id.to_string()}
                    size={IdentSize::Sm}
                    presence={peer_presence}
                    zoom={crate::components::common::Zoom {
                        title: title.clone(),
                        subtitle: None,
                        address: None,
                    }}
                />
                <div class="fn-chat__title fn-grow">
                    <span>{ &title }</span>
                    <PresenceLabel status={peer_presence} />
                    if room.has_encryption {
                        <Lock pending={rotation_pending} />
                    }
                    if is_admin {
                        <Badge variant="admin">{ t(lang, Key::admin) }</Badge>
                    }
                    <div class="fn-chat__submeta">
                        <button
                            type="button"
                            class="topcoat-button--quiet"
                            onclick={{
                                let on_navigate = p.on_navigate.clone();
                                let id = p.room_id.clone();
                                Callback::from(move |_: MouseEvent| {
                                    on_navigate.emit(Route::Members(id.clone()))
                                })
                            }}
                        >
                            { t(lang, if room.member_count == 1 {
                                    Key::member_count_one
                                } else {
                                    Key::member_count_many
                                }).replace("{n}", &room.member_count.to_string()) }
                        </button>
                        <ConnPill status={store.conn} onclick={{
                            let store = store.clone();
                            Callback::from(move |_: MouseEvent| {
                                let next = match store.mode {
                                    crate::session::ConnectionMode::Polling => {
                                        crate::session::ConnectionMode::WebSocket
                                    }
                                    _ => crate::session::ConnectionMode::Polling,
                                };
                                store.dispatch(Action::SetMode(next));
                            })
                        }} />
                    </div>
                </div>
                <div class="fn-chat__actions">
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::sync_this_room)}
                        title={t(lang, Key::sync_now)}
                        onclick={{
                            let cb = p.on_refresh.clone();
                            let id = p.room_id.clone();
                            Callback::from(move |_: MouseEvent| cb.emit(id.clone()))
                        }}
                    >{ icons::refresh(18) }</button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::room_actions)}
                        aria-expanded={menu_open.to_string()}
                        onclick={{
                            let menu_open = menu_open.clone();
                            Callback::from(move |_: MouseEvent| menu_open.set(!*menu_open))
                        }}
                    >{ icons::more(18) }</button>
                </div>
            </header>

            // Rendered unconditionally: `Popover` needs to see `open` turn
            // false to run the exit, which it cannot do if the parent has
            // already stopped rendering it.
            { room_menu(lang, &store, &room, &title, is_admin, menu_open.clone(), &p.on_navigate) }

            if offline {
                <super::common::OfflineBanner />
            }

            // One source of truth with the composer. Both render from the same
            // `PostBlock`, so they cannot name different remedies — which is
            // exactly what happened when each derived its own answer.
            if let Some(reason) = composer_blocked {
                { block_banner(lang, reason, *rotating, &on_rotate, &p.on_navigate) }
            }
            if let Some(e) = &*rotate_error {
                <Banner variant="danger">{ e.clone() }</Banner>
            }
            // Informational only — shown when nothing is blocking, to explain
            // history that will stay sealed.
            if room.has_encryption && composer_blocked.is_none() {
                { key_banner(lang, bundle.as_deref()) }
            }

            // The wrapper exists so the jump pill has a positioned ancestor
            // that is *not* the scroller — anchored to the stream itself it
            // would scroll away with the content it is offering to escape.
            // It also keeps `.fn-chat`'s four grid rows intact.
            <div class="fn-stream-wrap">
            <div
                ref={stream_ref}
                class="fn-stream fn-scroll"
                role="log"
                aria-live="polite"
                aria-relevant="additions"
                aria-label={t(lang, Key::messages_in_room).replace("{room}", &title)}
                onscroll={on_stream_scroll}
            >
                { for rows.into_iter() }
            </div>
            // The way back down. Only while scrolled away *and* something has
            // arrived since — a permanent "jump to bottom" control is one more
            // thing on screen answering a question nobody asked.
            if *unseen > 0 {
                <button
                    type="button"
                    class="fn-jump-latest"
                    onclick={jump_to_latest}
                >
                    { icons::back(14) }
                    <span>{ t(lang, if *unseen == 1 {
                                Key::new_message_one
                            } else {
                                Key::new_message_many
                            }).replace("{n}", &unseen.to_string()) }</span>
                </button>
            }
            </div>

            <div class="fn-typing" aria-live="polite" aria-atomic="true">
                if let Some(label) = format::typing_label(&typists) {
                    <span class="fn-typing__dots" aria-hidden="true"><i/><i/><i/></span>
                    { label }
                }
            </div>

            // Unconditional, like the two menus: the picker animates out, and
            // `Popover` cannot see a close it was unmounted before. This
            // instance serves only the composer button — a message's react
            // button opens the picker *inside its own row* (message.rs), so
            // it appears where the pointer already is.
            <Picker
                open={*picker_for == Some(None)}
                on_close={{
                    let picker_for = picker_for.clone();
                    Callback::from(move |_: ()| picker_for.set(None))
                }}
                on_pick={{
                    let picker_for = picker_for.clone();
                    Callback::from(move |_code: String| picker_for.set(None))
                }}
            />

            <Composer
                members={room.members.clone()}
                me={Some(me.clone())}
                replying_to={(*reply_to).as_ref().and_then(|id| {
                    // Name the message being replied to by its author, which
                    // is what somebody looking at the chip needs to recognise.
                    state.messages.get(id).map(|m| m.sender
                        .as_ref()
                        .map(|u| u.display_name())
                        .unwrap_or_else(|| m.sender_address.abbreviated()))
                })}
                on_cancel_reply={cancel_reply}
                room_name={title.clone()}
                blocked={composer_blocked}
                {offline}
                on_send={on_send}
                on_typing={{
                    let cb = p.on_typing.clone();
                    let id = p.room_id.clone();
                    Callback::from(move |_: ()| cb.emit(id.clone()))
                }}
                on_open_picker={{
                    let picker_for = picker_for.clone();
                    Callback::from(move |_: ()| picker_for.set(Some(None)))
                }}
                on_open_files={{
                    let store = store.clone();
                    let id = p.room_id.clone();
                    Callback::from(move |_: ()| {
                        store.dispatch(Action::OpenModal(crate::state::Modal::Files(id.clone())));
                    })
                }}
                on_attach={{
                    let store = store.clone();
                    let id = p.room_id.clone();
                    Callback::from(move |(file, caption): (web_sys::File, String)| {
                        let store = store.clone();
                        let id = id.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            crate::actions::attach_file(store, id, file, caption).await;
                        });
                    })
                }}
                on_open_assistant={{
                    let store = store.clone();
                    let id = p.room_id.clone();
                    Callback::from(move |_: ()| {
                        store.dispatch(Action::OpenModal(
                            crate::state::Modal::Assistant(id.clone()),
                        ));
                    })
                }}
            />
        </div>
    }
}

/// The banner for a state that is *blocking* the user, rendered from the same
/// [`PostBlock`] the composer uses.
///
/// Keeping the text and the action on `PostBlock` rather than inline here is
/// what makes the banner and the composer impossible to contradict: adding a
/// variant forces both to be answered in one place.
fn block_banner(
    lang: Lang,
    reason: PostBlock,
    rotating: bool,
    on_rotate: &Callback<MouseEvent>,
    on_navigate: &Callback<Route>,
) -> Html {
    let actions = match reason.banner_action() {
        None => None,
        Some(label) if reason == PostBlock::RotationPending => Some(html! {
            <BusyButton
                label={label.to_string()}
                class="topcoat-button"
                busy={rotating}
                onclick={on_rotate.clone()}
            />
        }),
        Some(label) => {
            let nav = on_navigate.clone();
            Some(html! {
                <button
                    type="button"
                    class="topcoat-button--cta"
                    onclick={Callback::from(move |_: MouseEvent| nav.emit(Route::Login))}
                >{ label }</button>
            })
        }
    };

    html! {
        <Banner variant="warn" {actions}>
            if rotating && reason == PostBlock::RotationPending {
                { t(lang, Key::rotating_keys) }
            } else {
                { reason.banner_text() }
            }
        </Banner>
    }
}

/// Explain history that will stay sealed even though nothing is blocking now.
fn key_banner(lang: Lang, bundle: Option<&crate::crypto::RoomKeyBundle>) -> Html {
    match bundle {
        Some(b) if !b.failed_epochs().is_empty() => html! {
            <Banner variant="warn">
                { t(lang, if b.failed_epochs().len() == 1 {
                        Key::sealed_keys_one
                    } else {
                        Key::sealed_keys_many
                    }).replace("{n}", &b.failed_epochs().len().to_string()) }
            </Banner>
        },
        _ => html! {},
    }
}

fn empty_stream(lang: Lang, load: &Load, encrypted: bool) -> Html {
    match load {
        Load::Loading => html! {
            <div class="fn-row"><Spinner large=true />{ t(lang, Key::opening_room) }</div>
        },
        Load::Error(e) => html! {
            <Empty art="⚠️" title={t(lang, Key::couldnt_load_room)} description={e.clone()} is_error=true
                   art_class="fn-art--offline" />
        },
        _ => html! {
            <Empty
                art="💬"
                title={t(lang, Key::no_messages_yet)}
                // An encrypted room's first screen is the one place the
                // encryption mark is worth showing at full size.
                art_class={if encrypted { "fn-art--encrypted" } else { "fn-art--messages" }}
                description={if encrypted {
                    "Say something — it'll be encrypted end to end."
                } else {
                    "Say something."
                }}
            />
        },
    }
}

/// An optimistic bubble for a message the server has not acknowledged.
fn pending_bubble(
    lang: Lang,
    q: &crate::state::Pending,
    store: &crate::state::Store,
    room_id: &RoomId,
) -> Html {
    let failed = q.failed.clone();
    let class = if failed.is_some() {
        "fn-msg fn-msg--own fn-msg--failed"
    } else {
        "fn-msg fn-msg--own fn-msg--pending"
    };
    let retry = {
        let store = store.clone();
        let room_id = room_id.clone();
        let local_id = q.local_id;
        let text = q.plaintext.clone();
        Callback::from(move |_: MouseEvent| {
            store.dispatch(Action::RetryPending(room_id.clone(), local_id));
            let store = store.clone();
            let room_id = room_id.clone();
            let text = text.clone();
            wasm_bindgen_futures::spawn_local(async move {
                actions::send_message(store, room_id, local_id, text, Default::default()).await;
            });
        })
    };
    let discard = {
        let store = store.clone();
        let room_id = room_id.clone();
        let local_id = q.local_id;
        Callback::from(move |_: MouseEvent| {
            store.dispatch(Action::DiscardPending(room_id.clone(), local_id))
        })
    };

    html! {
        <article {class} key={q.local_id}>
            <div class="fn-bubble">{ &q.plaintext }</div>
            <footer class="fn-msg__foot">
                if let Some(why) = failed {
                    <span title={why.clone()}>{ t(lang, Key::not_sent) }</span>
                    <button type="button" class="topcoat-button--quiet" onclick={retry}>{ t(lang, Key::retry) }</button>
                    <button type="button" class="topcoat-button--quiet" onclick={discard}>{ t(lang, Key::delete) }</button>
                } else {
                    <span>{ t(lang, Key::sending) }</span>
                }
            </footer>
        </article>
    }
}

/// The `⋮` menu. Destructive items open a real dialog — never `window.confirm`
/// — and none of them reloads the page.
fn room_menu(
    lang: Lang,
    store: &crate::state::Store,
    room: &crate::api::RoomWithMembers,
    title: &str,
    is_admin: bool,
    open: UseStateHandle<bool>,
    on_navigate: &Callback<Route>,
) -> Html {
    let is_open = *open;
    let id = room.id().clone();
    let name = title.to_owned();
    let direct = room.is_direct();

    let item = |label: &'static str, action: Modal, open: UseStateHandle<bool>| {
        let store = store.clone();
        let onclick = Callback::from(move |_: MouseEvent| {
            store.dispatch(Action::OpenModal(action.clone()));
            open.set(false);
        });
        html! {
            <button type="button" role="menuitem" class="topcoat-button--quiet" {onclick}>
                { label }
            </button>
        }
    };

    // `destructive` is purely presentational: it tints the item and draws
    // the separator above the first one, so "Delete room" does not sit in
    // the same colour as "Invite people".
    let confirm = |label: &'static str,
                   destructive: bool,
                   title: String,
                   body: String,
                   verb: String,
                   action: ConfirmAction,
                   open: UseStateHandle<bool>| {
        let store = store.clone();
        let onclick = Callback::from(move |_: MouseEvent| {
            // This removal came from the menu, so the swipe streak is broken
            // — "three in a row" is a claim about the gesture, and somebody
            // who came back to the menu has not made it.
            super::room_list::reset_swipe_streak();
            store.dispatch(Action::OpenModal(Modal::Confirm(Confirm {
                title: title.clone(),
                body: body.clone(),
                confirm_label: verb.clone(),
                action: action.clone(),
                alternative: None,
                challenge: None,
            })));
            open.set(false);
        });
        let class = classes!(
            "topcoat-button--quiet",
            destructive.then_some("fn-menuitem--danger")
        );
        html! {
            <button type="button" role="menuitem" {class} {onclick}>
                { label }
            </button>
        }
    };

    html! {
        <Popover open={is_open} class="fn-picker" role="menu" label={t(lang, Key::room_actions)}
            on_dismiss={{ let open = open.clone(); Callback::from(move |_: ()| open.set(false)) }}>
            // Everything gated on `!direct` is a channel verb the server
            // refuses for a DM: there is nobody to invite into a private
            // conversation, no name anybody chose to change, and every member
            // of a DM is already an admin of it. Offering a control that
            // always errors is worse than not offering it.
            if !direct && (is_admin || room.has_encryption) {
                { item(t(lang, Key::invite_people), Modal::Invite(id.clone()), open.clone()) }
            }
            if !direct && is_admin {
                // Share-by-link sits beside share-by-address: the first is for
                // people whose wallet address nobody has yet.
                { item(t(lang, Key::invite_links), Modal::InviteLinks(id.clone()), open.clone()) }
                { item(t(lang, Key::rename_room), Modal::RenameRoom(id.clone(), name.clone()), open.clone()) }
                { item(t(lang, Key::manage_admins), Modal::ManageAdmins(id.clone()), open.clone()) }
            }
            // Only where the server would say yes: a webhook holds no room
            // key, so an encrypted room has nothing to manage behind this
            // item and offering it would open a dialog that only errors.
            if !direct && is_admin && !room.has_encryption {
                { item(t(lang, Key::webhooks_menu), Modal::Webhooks(id.clone()), open.clone()) }
            }
            <button
                type="button"
                role="menuitem"
                class="topcoat-button--quiet"
                onclick={{
                    let on_navigate = on_navigate.clone();
                    let id = id.clone();
                    let open = open.clone();
                    Callback::from(move |_: MouseEvent| {
                        on_navigate.emit(Route::Members(id.clone()));
                        open.set(false);
                    })
                }}
            >{ t(lang, Key::view_members) }</button>
            // The gallery is a place, not an action, so it navigates exactly
            // as the roster does rather than opening a modal.
            <button
                type="button"
                role="menuitem"
                class="topcoat-button--quiet"
                onclick={{
                    let on_navigate = on_navigate.clone();
                    let id = id.clone();
                    let open = open.clone();
                    Callback::from(move |_: MouseEvent| {
                        on_navigate.emit(Route::Gallery(id.clone()));
                        open.set(false);
                    })
                }}
            >{ t(lang, Key::gallery_open) }</button>

            // Leaving is a channel verb too — a departed member would leave a
            // DM still keyed to their name, which they could then never
            // re-open. Hiding, below, is the reversible answer for both.
            if !direct {
                // An admin's exit asks a second question — see
                // `ConfirmAction::ExitAsAdmin`. The first dialog says so, so
                // that "Leave" is never a button whose consequences arrive
                // unannounced.
                { confirm(t(lang, Key::leave_room), false,
                          t(lang, Key::leave_room_title).replace("{name}", &name),
                          t(lang, if is_admin { Key::leave_room_admin_body } else { Key::leave_room_body }).to_owned(),
                          t(lang, Key::leave_room).to_owned(),
                          if is_admin {
                              ConfirmAction::ExitAsAdmin(id.clone())
                          } else {
                              ConfirmAction::LeaveRoom(id.clone())
                          },
                          open.clone()) }
            }
            { confirm(t(lang, Key::hide_room), false,
                      t(lang, Key::hide_room_title).replace("{name}", &name),
                      t(lang, Key::hide_room_body).to_owned(),
                      t(lang, Key::hide_room).to_owned(),
                      ConfirmAction::HideRoom(id.clone()), open.clone()) }
            if is_admin {
                { confirm(t(lang, Key::delete_all_messages), true,
                          t(lang, Key::delete_all_title).replace("{name}", &name),
                          t(lang, Key::delete_all_body).to_owned(),
                          t(lang, Key::delete_all).to_owned(),
                          ConfirmAction::DeleteAllMessages(id.clone()), open.clone()) }
                { confirm(t(lang, Key::delete_room), true,
                          t(lang, Key::delete_room_title).replace("{name}", &name),
                          t(lang, Key::delete_room_body).to_owned(),
                          t(lang, Key::delete_room).to_owned(),
                          ConfirmAction::DeleteRoom(id), open) }
            }
        </Popover>
    }
}

fn back_to_rooms(lang: Lang, on_navigate: &Callback<Route>) -> Html {
    let onclick = {
        let on_navigate = on_navigate.clone();
        Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Rooms))
    };
    html! { <button type="button" class="topcoat-button--cta" {onclick}>{ t(lang, Key::back_to_rooms) }</button> }
}

/// The detail pane with no room selected.
#[function_component(NoRoom)]
pub fn no_room() -> Html {
    let lang = crate::state::use_store().language;
    html! {
        // Its own artwork, not `fn-art--rooms`: on a first sign-in this pane
        // sits beside the room list's empty state, and the same illustration
        // twice on one screen reads as a rendering bug, not as two states.
        <Empty
            art="💬"
            title={t(lang, Key::pick_a_room)}
            art_class="fn-art--pick"
            description={t(lang, Key::pick_a_room_body)}
        />
    }
}

/// A message's readable body, decrypted at most once per version.
///
/// The cache is a plain map behind a `RefCell` rather than anything cleverer:
/// it is read and written only from the render of one component, on one
/// thread, and the entry it wants is the one keyed by the id in hand.
fn decrypt_cached(
    cache: &std::rc::Rc<std::cell::RefCell<HashMap<MessageId, (i64, Decrypted)>>>,
    bundle: &Option<std::rc::Rc<crate::crypto::RoomKeyBundle>>,
    room_id: &RoomId,
    m: &Message,
) -> Decrypted {
    if let Some((serial, body)) = cache.borrow().get(&m.id) {
        if *serial == m.msg_serial {
            return body.clone();
        }
    }
    let body = match bundle {
        Some(b) => decrypt_message(b, room_id, m),
        None if m.is_encrypted => Decrypted::NoKeyForEpoch(m.key_version()),
        None => Decrypted::Plaintext(m.content.clone()),
    };
    cache
        .borrow_mut()
        .insert(m.id.clone(), (m.msg_serial, body.clone()));
    body
}

/// Settle the stream on the newest message.
///
/// `smooth` distinguishes the two cases, and they are genuinely different: a
/// message *arriving* wants the animation, because a bubble that simply
/// appears below the fold reads as a jump cut and the motion is what says
/// "this new thing is now at the bottom". *Opening a room* does not — the
/// backlog was already there, and animating three thousand pixels of it past
/// the reader is a smear, not a transition.
///
/// The behaviour is always passed **explicitly**, never left to the
/// stylesheet. `.fn-scroll` carries `scroll-behavior: smooth`, which silently
/// animates a bare `scrollTop` assignment too — so the "instant" path was
/// instant only in the source until this was spelled out. Options-level
/// behaviour overrides the CSS property, which is what makes this reliable.
///
/// Smooth collapses to instant under `prefers-reduced-motion`. Not a nicety:
/// a scroll animation is exactly the vestibular trigger that setting exists
/// for, and the stylesheet already forces `scroll-behavior: auto` there.
#[cfg(target_arch = "wasm32")]
fn scroll_to_latest(el: &HtmlElement, smooth: bool) {
    // Go to the last row, not to a computed height.
    //
    // `scrollTop = scrollHeight` is arithmetic over every row above, and it is
    // stale the instant one of them is not yet the height it will become —
    // which is every row carrying a video or an image, since they render
    // before the poster decodes. Asking the browser to bring the *last
    // element* into view carries no such assumption: whatever is above it,
    // "the last one, at the bottom" means the same thing however tall the
    // rows turn out to be.
    let Some(last) = el.last_element_child() else {
        return;
    };

    // Animate a hop, jump a journey.
    //
    // "Is this the first settle?" turned out to be the wrong question: a
    // room's backlog does not arrive in one count change. It lands in waves
    // (`/messages` backfill, then `/sync`, then the DOM painting the rows),
    // so the flag was spent on a partial stream and the *rest* of the backlog
    // then got the smooth path — three thousand pixels of history sliding
    // past on every room open.
    //
    // Distance is the honest signal. More than two screens to cover is a
    // load, not an arrival, and nobody wants to watch it; a message or two is
    // an arrival, and the motion is what says the new bubble is the one now
    // at the bottom. This also stops the jump pill from smearing a reader
    // back down from the top of a long history.
    let travel = el.scroll_height() as f64 - el.scroll_top() as f64 - el.client_height() as f64;
    let long_haul = travel > (el.client_height() as f64) * 2.0;

    // `scrollIntoView` alone stops at the row's own bottom edge and leaves the
    // container's bottom padding uncovered — measurably 8px short of the end
    // here. So the last row decides *whether* there is anywhere to go, and the
    // scroll itself targets the true bottom. That keeps the property the
    // suggestion was after — never compute a height from the rows above — while
    // still landing flush.
    let _ = &last;

    if smooth && !long_haul && !reduced_motion() {
        let opts = web_sys::ScrollToOptions::new();
        opts.set_top(el.scroll_height() as f64);
        opts.set_behavior(web_sys::ScrollBehavior::Smooth);
        el.scroll_to_with_scroll_to_options(&opts);
        return;
    }

    // Jumping needs an *inline* override, not `behavior: "auto"`.
    //
    // `.fn-scroll` carries `scroll-behavior: smooth`, and that CSS property
    // animates `scrollIntoView` and a bare `scrollTop` assignment alike.
    // Passing `behavior: "auto"` in the options is supposed to win and —
    // measured in this Chromium, not assumed — does not: the container kept
    // animating, which is why "instant" room-opens were still smearing three
    // thousand pixels of backlog past the reader. An inline style outranks
    // the stylesheet, so the property is switched off for the call and put
    // back immediately.
    let style = el.style();
    let previous = style
        .get_property_value("scroll-behavior")
        .unwrap_or_default();
    let _ = style.set_property("scroll-behavior", "auto");
    el.set_scroll_top(el.scroll_height());
    if previous.is_empty() {
        let _ = style.remove_property("scroll-behavior");
    } else {
        let _ = style.set_property("scroll-behavior", &previous);
    }
}

/// Host builds have no DOM; the component is only rendered under wasm.
#[cfg(not(target_arch = "wasm32"))]
fn scroll_to_latest(_el: &HtmlElement, _smooth: bool) {}

#[cfg(target_arch = "wasm32")]
fn reduced_motion() -> bool {
    web_sys::window()
        .and_then(|w| {
            w.match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|m| m.matches())
}
