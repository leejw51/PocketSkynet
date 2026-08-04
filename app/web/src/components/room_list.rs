//! Screen 2 — Room list (DESIGN.md §6).
//!
//! Two corrections to the reference client, both visible in the row anatomy:
//! it renders neither the last-message preview nor a timestamp even though the
//! API returns both, and it lists rooms in database insertion order. This one
//! renders both and sorts by last activity — a room list in insertion order is
//! not a room list.
//!
//! Keyboard: the list is a `listbox` with roving tabindex, so it is a single
//! tab stop; ↑/↓ move, Home/End jump, and typing a letter jumps to the next
//! room starting with it.

use pocketskynet_core::RoomId;
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, HtmlInputElement};
use yew::prelude::*;

use crate::actions;
use crate::api::RoomWithMembers;
use crate::format;
use crate::route::Route;
use crate::state::{use_store, Action, Confirm, ConfirmAction, KnowledgeSeed, Load, Modal, Store};

use super::common::{Badge, Empty, Ident, Lock, Skeleton, Unread};
use super::icons;
use crate::i18n::{t, Key, Lang};

#[derive(Properties, PartialEq)]
pub struct RoomListProps {
    pub selected: Option<RoomId>,
    pub on_navigate: Callback<Route>,
    pub on_reload: Callback<()>,
}

#[function_component(RoomList)]
pub fn room_list(p: &RoomListProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let query = use_state(String::new);
    let search_ref = use_node_ref();
    let list_ref = use_node_ref();
    // Which room's swipe drawer is open. One at a time, list-wide.
    let open_swipe = use_state(|| Option::<RoomId>::None);
    let on_swipe = {
        let open_swipe = open_swipe.clone();
        Callback::from(move |id: Option<RoomId>| open_swipe.set(id))
    };

    // Light dismiss for the swipe drawer. An armed row left open behind a
    // search or a scroll is a delete button lying in wait; anything that is
    // clearly *not* about that row puts it back.
    //
    // The listener arms on a macrotask for the same reason `Popover`'s does:
    // the click that opened the drawer is still bubbling towards the document
    // while this effect runs, and armed immediately it would close what it
    // just opened.
    {
        let open_swipe = open_swipe.clone();
        use_effect_with(open_swipe.is_some(), move |armed| {
            let listener = armed
                .then(|| {
                    let document = web_sys::window()?.document()?;
                    let live = std::rc::Rc::new(std::cell::Cell::new(false));
                    {
                        let live = live.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            gloo_timers::future::TimeoutFuture::new(0).await;
                            live.set(true);
                        });
                    }
                    Some(gloo_events::EventListener::new(
                        &document,
                        "pointerdown",
                        move |e: &web_sys::Event| {
                            if !live.get() {
                                return;
                            }
                            let inside = e
                                .target()
                                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                                .and_then(|el| el.closest("[data-open='true']").ok().flatten())
                                .is_some();
                            if !inside {
                                open_swipe.set(None);
                            }
                        },
                    ))
                })
                .flatten();
            move || drop(listener)
        });
    }

    // Ctrl/Cmd+K focuses search from anywhere in the app.
    {
        let search_ref = search_ref.clone();
        use_effect_with((), move |_| {
            let listener = gloo_events::EventListener::new(
                &web_sys::window().expect("a browser window"),
                "keydown",
                move |e| {
                    let Some(e) = e.dyn_ref::<KeyboardEvent>() else {
                        return;
                    };
                    if e.key() == "k" && (e.meta_key() || e.ctrl_key()) {
                        e.prevent_default();
                        if let Some(el) = search_ref.cast::<HtmlElement>() {
                            let _ = el.focus();
                        }
                    }
                },
            );
            move || drop(listener)
        });
    }

    let on_query = {
        let query = query.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                query.set(el.value());
            }
        })
    };

    let on_search_key = {
        let query = query.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Escape" {
                e.stop_propagation();
                query.set(String::new());
            }
        })
    };

    let open_create = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| store.dispatch(Action::OpenModal(Modal::CreateRoom)))
    };

    // The ⚡ shortcut: an encrypted room, named automatically, created and
    // opened from right here — no dialog. The same button exists inside the
    // create dialog, but a shortcut that first makes you open a dialog is not
    // a shortcut, and nothing on this screen said one-click was possible.
    let fast_busy = use_state(|| false);
    let fast_create = {
        let store = store.clone();
        let fast_busy = fast_busy.clone();
        let on_navigate = p.on_navigate.clone();
        Callback::from(move |_: MouseEvent| {
            if *fast_busy {
                return;
            }
            fast_busy.set(true);
            let store = store.clone();
            let fast_busy = fast_busy.clone();
            let on_navigate = on_navigate.clone();
            wasm_bindgen_futures::spawn_local(async move {
                fast_create_room(store, on_navigate).await;
                fast_busy.set(false);
            });
        })
    };

    let needle = query.trim().to_ascii_lowercase();
    let rooms: Vec<&RoomWithMembers> = store
        .sorted_rooms()
        .into_iter()
        .filter(|r| needle.is_empty() || r.room.name.to_ascii_lowercase().contains(&needle))
        .collect();

    // Roving tabindex: exactly one row is tabbable, and arrows move focus
    // between them. A list where every row is a tab stop is unusable with a
    // keyboard once it has more than about five entries.
    let onkeydown = {
        let list_ref = list_ref.clone();
        Callback::from(move |e: KeyboardEvent| {
            let Some(root) = list_ref.cast::<web_sys::Element>() else {
                return;
            };
            let Ok(rows) = root.query_selector_all("[role='option']") else {
                return;
            };
            let n = rows.length() as i32;
            if n == 0 {
                return;
            }
            let active = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.active_element());
            let mut current = -1i32;
            for i in 0..n {
                if let (Some(node), Some(active)) = (rows.item(i as u32), active.as_ref()) {
                    if node.is_same_node(Some(active.as_ref())) {
                        current = i;
                    }
                }
            }

            let target = match e.key().as_str() {
                "ArrowDown" => (current + 1).min(n - 1),
                "ArrowUp" => (current - 1).max(0),
                "Home" => 0,
                "End" => n - 1,
                key if key.len() == 1
                    && key.chars().next().is_some_and(|c| c.is_alphanumeric()) =>
                {
                    // Type-ahead: jump to the next row whose label starts with
                    // the typed character, wrapping around.
                    let want = key.to_ascii_lowercase();
                    let mut found = -1;
                    for step in 1..=n {
                        let i = (current + step).rem_euclid(n);
                        let starts = rows
                            .item(i as u32)
                            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
                            .and_then(|el| el.get_attribute("data-name"))
                            .is_some_and(|name| name.to_ascii_lowercase().starts_with(&want));
                        if starts {
                            found = i;
                            break;
                        }
                    }
                    if found < 0 {
                        return;
                    }
                    found
                }
                _ => return,
            };

            e.prevent_default();
            if let Some(el) = rows
                .item(target as u32)
                .and_then(|n| n.dyn_into::<HtmlElement>().ok())
            {
                let _ = el.focus();
            }
        })
    };

    let now = format::now_ms();
    let tz = format::tz_offset_minutes();
    let invites = store.pending_invitations();

    // The quick bar: one field over everything the server remembers
    // (docs/SEARCH.md). With an AI provider key on this device it *answers*
    // — labelled AI Search so nobody is surprised where the question goes;
    // without one it retrieves, and the label promises only that.
    let quick = use_state(String::new);
    let ai_ready = crate::ai::AiSettings::load().text_provider().is_some();
    let quick_submit = {
        let store = store.clone();
        let on_navigate = p.on_navigate.clone();
        let quick = quick.clone();
        Callback::from(move |_: ()| {
            let q = quick.trim().to_owned();
            if q.is_empty() {
                return;
            }
            store.dispatch(Action::SeedKnowledge(KnowledgeSeed::Search {
                query: q,
                ask: ai_ready,
            }));
            quick.set(String::new());
            on_navigate.emit(Route::Knowledge);
        })
    };

    html! {
        <>
            <div class="fn-quicksearch">
                <span class="fn-quicksearch__chip">
                    if ai_ready {
                        { icons::spark(12) }
                        { t(lang, Key::ai_search) }
                    } else {
                        { icons::search(12) }
                        { t(lang, Key::mode_search) }
                    }
                </span>
                <input
                    class="fn-quicksearch__input"
                    type="search"
                    placeholder={if ai_ready {
                        t(lang, Key::quick_search_placeholder_ai)
                    } else {
                        t(lang, Key::quick_search_placeholder)
                    }}
                    aria-label={if ai_ready {
                        t(lang, Key::ai_search)
                    } else {
                        t(lang, Key::mode_search)
                    }}
                    value={(*quick).clone()}
                    oninput={{
                        let quick = quick.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                                quick.set(el.value());
                            }
                        })
                    }}
                    onkeydown={{
                        let quick_submit = quick_submit.clone();
                        Callback::from(move |e: KeyboardEvent| {
                            if e.key() == "Enter" {
                                e.prevent_default();
                                quick_submit.emit(());
                            }
                        })
                    }}
                />
                <button
                    type="button"
                    class="topcoat-icon-button--quiet fn-quicksearch__go"
                    aria-label={if ai_ready { t(lang, Key::ai_search) } else { t(lang, Key::mode_search) }}
                    onclick={{
                        let quick_submit = quick_submit.clone();
                        Callback::from(move |_: MouseEvent| quick_submit.emit(()))
                    }}
                >
                    { icons::search(16) }
                </button>
            </div>
            <div class="fn-roomlist__head">
                <input
                    ref={search_ref}
                    class="topcoat-search-input"
                    type="search"
                    placeholder={t(lang, Key::search_rooms)}
                    aria-label={t(lang, Key::search_rooms)}
                    value={(*query).clone()}
                    oninput={on_query}
                    onkeydown={on_search_key}
                />
                if store.online {
                    // ⚡ before +: the one-click path is the common case, the
                    // form is the configurable one. Cyan-tinted so it reads as
                    // the quick action rather than a second identical button.
                    <button
                        type="button"
                        class="topcoat-icon-button fn-fastbtn"
                        aria-label={t(lang, Key::fast_create_hint)}
                        title={t(lang, Key::fast_create_hint)}
                        disabled={*fast_busy}
                        onclick={fast_create.clone()}
                    >
                        { icons::bolt(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button"
                        aria-label={t(lang, Key::create_room_dots)}
                        title={t(lang, Key::create_room_dots)}
                        onclick={open_create}
                    >
                        { icons::plus(18) }
                    </button>
                } else {
                    // Offline: the create buttons are replaced by the state
                    // that explains why they are gone.
                    <span class="fn-conn fn-conn--offline">{ t(lang, Key::offline) }</span>
                }
            </div>

            <div class="fn-roomlist__body fn-scroll">
                if invites > 0 {
                    <button
                        type="button"
                        class="fn-room-row"
                        onclick={{
                            let on_navigate = p.on_navigate.clone();
                            Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Invitations))
                        }}
                    >
                        <span class="fn-room-row__avatar" aria-hidden="true">
                            { icons::envelope(20) }
                        </span>
                        <span class="fn-room-row__title">
                            <span class="fn-room-row__name">{ t(lang, Key::invitations) }</span>
                        </span>
                        <span class="fn-room-row__aside"><Unread count={invites} /></span>
                    </button>
                }

                { body(lang, &store, &rooms, p, &needle, now, tz, list_ref, onkeydown,
                       (*open_swipe).clone(), on_swipe) }
            </div>
        </>
    }
}

#[allow(clippy::too_many_arguments)]
fn body(
    lang: Lang,
    store: &crate::state::Store,
    rooms: &[&RoomWithMembers],
    p: &RoomListProps,
    needle: &str,
    now: i64,
    tz: i32,
    list_ref: NodeRef,
    onkeydown: Callback<KeyboardEvent>,
    open_swipe: Option<RoomId>,
    on_swipe: Callback<Option<RoomId>>,
) -> Html {
    match &store.rooms_load {
        // `Idle` renders nothing at all: a loader before 400 ms is a flash, and
        // a flash reads as a bug (DESIGN.md §15).
        Load::Idle => html! {},
        Load::Loading if store.rooms.is_empty() => html! {
            <div class="fn-roomlist__loading"><Skeleton rows={6} /></div>
        },
        Load::Error(e) if store.rooms.is_empty() => html! {
            <Empty art="⚠️" title={t(lang, Key::couldnt_load_rooms)} description={e.clone()} is_error=true
                   art_class="fn-art--offline">
                <button
                    type="button"
                    class="topcoat-button--cta"
                    onclick={{
                        let on_reload = p.on_reload.clone();
                        Callback::from(move |_: MouseEvent| on_reload.emit(()))
                    }}
                >{ t(lang, Key::try_again) }</button>
            </Empty>
        },
        _ if rooms.is_empty() && !needle.is_empty() => html! {
            <Empty art="🔍" title={t(lang, Key::no_rooms_match).replace("{query}", needle)}
                   art_class="fn-art--search"
                   description={t(lang, Key::no_search_results)} />
        },
        _ if rooms.is_empty() => html! {
            <Empty art="💬" title={t(lang, Key::no_rooms_yet)} art_class="fn-art--rooms"
                   description={t(lang, Key::no_rooms_body)}>
                // The same hierarchy as the create dialog: one click first,
                // the form as the alternative.
                <button
                    type="button"
                    class="topcoat-button--cta"
                    onclick={{
                        let store = store.clone();
                        let on_navigate = p.on_navigate.clone();
                        Callback::from(move |_: MouseEvent| {
                            let store = store.clone();
                            let on_navigate = on_navigate.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                fast_create_room(store, on_navigate).await;
                            });
                        })
                    }}
                >{ icons::bolt(16) }{ t(lang, Key::fast_create_room) }</button>
                <button
                    type="button"
                    class="topcoat-button"
                    onclick={{
                        let store = store.clone();
                        Callback::from(move |_: MouseEvent| {
                            store.dispatch(Action::OpenModal(Modal::CreateRoom))
                        })
                    }}
                >{ t(lang, Key::create_room_setup) }</button>
            </Empty>
        },
        _ => {
            let me = store.me().cloned();
            html! {
                <div
                    ref={list_ref}
                    role="listbox"
                    aria-label={t(lang, Key::nav_rooms)}
                    tabindex="-1"
                    {onkeydown}
                >
                    { for rooms.iter().enumerate().map(|(i, r)| {
                        let selected = p.selected.as_ref() == Some(r.id());
                        // The server's count is authoritative but is not
                        // block-filtered and lags a live batch; fall back to the
                        // locally folded count when it is absent.
                        let unread = r.unread_count.or_else(|| {
                            let me = me.as_ref()?;
                            Some(store.room_state(r.id())?.local_unread(me, &store.blocks))
                        }).unwrap_or(0);
                        html! {
                            <RoomRow
                                key={r.id().to_string()}
                                room={(*r).clone()}
                                // `--i` drives the CSS entrance stagger
                                // (app.css §14); the delay is capped there.
                                index={i}
                                selected={selected}
                                // Roving tabindex: the selected row, or the first.
                                tabbable={selected || (p.selected.is_none() && i == 0)}
                                unread={unread}
                                is_admin={me.as_ref().is_some_and(|m| r.is_admin(m))}
                                now={now}
                                tz={tz}
                                lang={lang}
                                on_navigate={p.on_navigate.clone()}
                                open_swipe={open_swipe.clone()}
                                on_swipe={on_swipe.clone()}
                            />
                        }
                    }) }
                </div>
            }
        }
    }
}

/// The ⚡ shortcut's whole journey: auto-name, create encrypted, greet, open.
///
/// Refuses up front when the keys are locked — the button promises an
/// *encrypted* room, and a locked session can only make plaintext ones. The
/// dialog's fast button makes the same check for the same reason.
///
/// A downgrade after creation (`Ok((id, Some(why)))`) still navigates — the
/// room exists and hiding it would be worse — but arrives with a sticky error
/// toast naming the reason, because a silently plaintext "encrypted room" is
/// the one outcome this feature must never produce (DESIGN.md §8).
async fn fast_create_room(store: Store, on_navigate: Callback<Route>) {
    use super::toast;

    if !store.auth.can_decrypt() {
        toast::error(
            &store,
            t(store.language, Key::unlock_wallet_first),
            Some(
                "A fast room is always encrypted, and encryption needs your recovery phrase."
                    .into(),
            ),
        );
        return;
    }

    let (name, description) = actions::auto_room(store.language);
    match actions::fast_create_room(&store, &name, &description).await {
        Ok((room_id, None)) => {
            toast::success(
                &store,
                t(store.language, Key::room_created_named).replace("{name}", &name),
            );
            on_navigate.emit(Route::Room(room_id));
        }
        Ok((room_id, Some(why))) => {
            toast::error(
                &store,
                "Room created without encryption",
                Some(format!("{why}.")),
            );
            on_navigate.emit(Route::Room(room_id));
        }
        Err(e) => toast::error(&store, t(store.language, Key::couldnt_create_room), Some(e)),
    }
}

/// Swipe-to-remove, and the shortcut it teaches.
///
/// # The gesture
///
/// A drag left past 40 % of the drawer opens it; the buttons are then a
/// deliberate second tap. That is the safe version, and it is the version
/// everybody gets on their first room.
///
/// # The shortcut
///
/// Three removals in a row by swipe and the drawer stops being a required
/// stop: a drag that carries past the whole drawer plus a margin goes straight
/// to the confirmation. The threshold is *distance*, not speed, so it cannot
/// be reached by a flick that got away — and the confirmation is still there,
/// because the point of the escalation is to remove a tap, not a decision.
///
/// The streak resets when a room is removed from the `⋮` menu instead
/// ([`reset_streak`], called from `chat.rs`): "in a row" is a claim about the
/// gesture, and somebody who went back to the menu has not made it.
///
/// # Why the transform is written to the DOM
///
/// A pointermove that goes through the store is a re-render of the room list
/// per frame of a finger movement. The offset is set on the track node
/// directly, and only the *outcome* — which drawer is open — becomes state.
mod swipe {
    use std::cell::Cell;

    use web_sys::PointerEvent;
    use yew::NodeRef;

    use crate::api::RoomWithMembers;
    use crate::i18n::{t, Key, Lang};
    use crate::state::{Action, Confirm, ConfirmAction, Modal, Store};

    /// Removals in a row before a full swipe stops needing the buttons.
    const EXPRESS_AFTER: u32 = 3;
    const KEY_STREAK: &str = "ps-swipe-streak";
    /// Set once the shortcut has been announced, so the tip is a moment rather
    /// than a recurring notification.
    const KEY_TAUGHT: &str = "ps-swipe-taught";

    /// Fraction of the drawer that has to be crossed for it to stay open.
    const OPEN_AT: f64 = 0.4;
    /// Past the drawer *and* this much more, for the express removal. Wide
    /// enough that opening the drawer and overshooting is not a removal.
    const EXPRESS_MARGIN: f64 = 64.0;
    /// How far the drag may run past the express point, so the row still gives
    /// under the finger instead of hitting a wall.
    const OVERDRAG: f64 = 48.0;
    /// Movement before the drag commits to an axis. Below this a touch is
    /// still a tap, and a scroll is still a scroll.
    const SLOP: f64 = 10.0;

    thread_local! {
        /// Whether the removal now in flight was started by a swipe. Read by
        /// `app.rs` once the server has actually answered — a streak counted
        /// at the moment of *intent* would grow on confirmations the user
        /// backed out of.
        static FROM_SWIPE: Cell<bool> = const { Cell::new(false) };
    }

    /// Which axis a drag has committed to, once it has moved far enough to
    /// have an opinion.
    #[derive(Clone, Copy, PartialEq, Default)]
    enum Axis {
        #[default]
        Undecided,
        Horizontal,
        /// The finger is scrolling the list. Nothing else happens for the rest
        /// of this gesture.
        Vertical,
    }

    #[derive(Default)]
    pub struct Drag {
        active: bool,
        axis: Axis,
        x0: f64,
        y0: f64,
        /// Where the row already was when the finger landed — a drawer that is
        /// open drags from its open position, not from zero.
        base: f64,
        width: f64,
        offset: f64,
        /// A completed horizontal drag ends in a `click` the browser still
        /// intends to deliver. This swallows exactly one.
        click_guard: bool,
    }

    pub enum Outcome {
        /// Not a horizontal drag: a tap, or a scroll. Leave everything alone.
        Idle,
        Closed,
        Opened(f64),
        Express,
    }

    impl Drag {
        pub fn start(&mut self, e: &PointerEvent, width: f64, is_open: bool) {
            // A touch that ends in a big movement does not always produce the
            // `click` the guard was armed for, and a guard left armed would
            // swallow the *next* honest tap instead. A new gesture is proof
            // the last one is over.
            self.click_guard = false;
            self.active = true;
            self.axis = Axis::Undecided;
            self.x0 = e.client_x() as f64;
            self.y0 = e.client_y() as f64;
            self.base = if is_open { -width } else { 0.0 };
            self.width = width;
            self.offset = self.base;
        }

        /// The offset to paint, or `None` while the drag has nothing to say.
        pub fn mv(&mut self, e: &PointerEvent) -> Option<f64> {
            if !self.active {
                return None;
            }
            let dx = e.client_x() as f64 - self.x0;
            let dy = e.client_y() as f64 - self.y0;
            if self.axis == Axis::Undecided {
                if dx.abs() < SLOP && dy.abs() < SLOP {
                    return None;
                }
                // Ties go to the list. A room list is scrolled far more often
                // than it is pruned, and a scroll that turns into a swipe is
                // the more annoying of the two mistakes.
                self.axis = if dx.abs() > dy.abs() {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                // Capture only once the gesture is ours, so a vertical drag
                // keeps belonging to the scroller.
                if self.axis == Axis::Horizontal {
                    if let Some(target) = e
                        .target()
                        .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
                    {
                        let _ = target.set_pointer_capture(e.pointer_id());
                    }
                }
            }
            if self.axis != Axis::Horizontal {
                return None;
            }
            // Leftward only, and never further than the express point plus a
            // little give. Rightward from a shut row would reveal nothing.
            let floor = -(self.width + EXPRESS_MARGIN + OVERDRAG);
            self.offset = (self.base + dx).clamp(floor, 0.0);
            Some(self.offset)
        }

        pub fn end(&mut self, e: &PointerEvent) -> Outcome {
            let was_horizontal = self.active && self.axis == Axis::Horizontal;
            self.active = false;
            self.axis = Axis::Undecided;
            if !was_horizontal {
                return Outcome::Idle;
            }
            self.click_guard = true;
            if let Some(target) = e
                .target()
                .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            {
                let _ = target.release_pointer_capture(e.pointer_id());
            }
            let travelled = -self.offset;
            if travelled >= self.width + EXPRESS_MARGIN && express_ready() {
                Outcome::Express
            } else if travelled >= self.width * OPEN_AT {
                Outcome::Opened(self.width)
            } else {
                Outcome::Closed
            }
        }

        /// Consumes the pending click suppression, if there is one.
        pub fn take_click_guard(&mut self) -> bool {
            std::mem::take(&mut self.click_guard)
        }
    }

    /// The drawer's measured width. Measured rather than declared: the labels
    /// are translated and the whole interface has a text-size setting, so the
    /// distance a finger has to travel is not a number this file can know.
    pub fn width(actions: &NodeRef) -> f64 {
        actions
            .cast::<web_sys::HtmlElement>()
            .map(|el| el.offset_width() as f64)
            // Before the first layout, a sane default rather than a zero that
            // would make every touch a full swipe.
            .filter(|w| *w > 1.0)
            .unwrap_or(152.0)
    }

    /// Put the track at `offset` px. `animate` is off while a finger is on it
    /// — a transition during a drag is the row lagging behind the thumb.
    pub fn slide(track: &NodeRef, offset: f64, animate: bool) {
        let Some(el) = track.cast::<web_sys::Element>() else {
            return;
        };
        let _ = el.set_attribute(
            "style",
            &format!("transform: translate3d({offset}px, 0, 0)"),
        );
        let _ = if animate {
            el.remove_attribute("data-dragging")
        } else {
            el.set_attribute("data-dragging", "true")
        };
    }

    pub fn set_dragging(track: &NodeRef, dragging: bool) {
        let Some(el) = track.cast::<web_sys::Element>() else {
            return;
        };
        let _ = if dragging {
            el.set_attribute("data-dragging", "true")
        } else {
            el.remove_attribute("data-dragging")
        };
    }

    pub fn hide_confirm(lang: Lang, r: &RoomWithMembers) -> Confirm {
        Confirm {
            title: t(lang, Key::hide_room_title).replace("{name}", &r.room.name),
            body: t(lang, Key::hide_room_body).into(),
            confirm_label: t(lang, Key::hide_room).into(),
            action: ConfirmAction::HideRoom(r.id().clone()),
        }
    }

    /// The express swipe's destination. Hiding, not leaving: it is the
    /// reversible one, and a gesture should not be able to cost you a room key.
    pub fn confirm_hide(store: &Store, r: &RoomWithMembers) {
        mark_intent();
        store.dispatch(Action::OpenModal(Modal::Confirm(hide_confirm(
            store.language,
            r,
        ))));
    }

    /// This removal came from the swipe. Cleared by [`settle`].
    pub fn mark_intent() {
        FROM_SWIPE.with(|c| c.set(true));
    }

    fn streak() -> u32 {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            LocalStorage::get(KEY_STREAK).unwrap_or(0)
        }
        #[cfg(not(target_arch = "wasm32"))]
        0
    }

    fn set_streak(_n: u32) {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            let _ = LocalStorage::set(KEY_STREAK, _n);
        }
    }

    fn express_ready() -> bool {
        streak() >= EXPRESS_AFTER
    }

    /// A room was removed from the `⋮` menu. The gesture streak is about the
    /// gesture; this breaks it.
    pub fn reset_streak() {
        FROM_SWIPE.with(|c| c.set(false));
        set_streak(0);
    }

    /// A room removal landed. Called from `app.rs` once the server has agreed,
    /// which is the only moment that counts.
    pub fn settle(store: &Store) {
        if !FROM_SWIPE.with(|c| c.replace(false)) {
            set_streak(0);
            return;
        }
        let n = streak().saturating_add(1);
        set_streak(n);
        // Announced once, at the moment it becomes true, and never again. A
        // shortcut you are reminded of every time is not a shortcut.
        if n == EXPRESS_AFTER && !taught() {
            set_taught();
            store.dispatch(Action::Toast(
                crate::state::ToastKind::Info,
                t(store.language, Key::swipe_shortcut_ready).into(),
                Some(t(store.language, Key::swipe_shortcut_ready_body).into()),
            ));
        }
    }

    fn taught() -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            LocalStorage::get(KEY_TAUGHT).unwrap_or(false)
        }
        #[cfg(not(target_arch = "wasm32"))]
        false
    }

    fn set_taught() {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            let _ = LocalStorage::set(KEY_TAUGHT, true);
        }
    }
}

pub use swipe::{reset_streak as reset_swipe_streak, settle as settle_swipe_streak};

#[derive(Properties, PartialEq)]
pub struct RoomRowProps {
    pub room: RoomWithMembers,
    pub index: usize,
    pub selected: bool,
    pub tabbable: bool,
    pub unread: u32,
    pub is_admin: bool,
    pub now: i64,
    pub tz: i32,
    pub lang: Lang,
    pub on_navigate: Callback<Route>,
    /// Which row's swipe drawer is open, if any. Held by the list rather than
    /// by each row so opening one closes the rest — two open drawers is two
    /// rooms armed for removal, and the second one is always the accident.
    pub open_swipe: Option<RoomId>,
    pub on_swipe: Callback<Option<RoomId>>,
}

#[function_component(RoomRow)]
fn room_row(p: &RoomRowProps) -> Html {
    let store = use_store();
    let lang = p.lang;
    let r = &p.room;
    let id = r.id().clone();
    let is_admin = p.is_admin;
    let unread = p.unread;
    let selected = p.selected;
    let rotating = r.room.key_rotation_pending;
    let is_open = p.open_swipe.as_ref() == Some(&id);

    let track = use_node_ref();
    let actions = use_node_ref();
    let drag = use_mut_ref(swipe::Drag::default);

    // The list owns the open row, so a drawer can be closed by something that
    // happened on a *different* row. The transform is written directly to the
    // node rather than rendered from state (see `swipe::slide`), so closing has
    // to be an effect rather than a prop.
    {
        let track = track.clone();
        use_effect_with(is_open, move |open| {
            if !*open {
                swipe::slide(&track, 0.0, true);
            }
            || ()
        });
    }

    let close_drawer = {
        let on_swipe = p.on_swipe.clone();
        let track = track.clone();
        move || {
            swipe::slide(&track, 0.0, true);
            on_swipe.emit(None);
        }
    };

    let mut class = classes!("fn-room-row");
    if unread > 0 {
        class.push("is-unread");
    }
    if rotating {
        class.push("is-rotation-pending");
    }

    let activate = {
        let on_navigate = p.on_navigate.clone();
        let id = id.clone();
        let drag = drag.clone();
        let close_drawer = close_drawer.clone();
        Callback::from(move |_: MouseEvent| {
            // A swipe ends in a `click` the browser still believes in. Left
            // unanswered it would open the very room the gesture was about to
            // remove.
            if drag.borrow_mut().take_click_guard() {
                return;
            }
            // With the drawer open the row is the way *out* of it: the first
            // tap puts the row back, it does not navigate. Same instinct as
            // tapping outside a menu.
            if is_open {
                close_drawer();
                return;
            }
            on_navigate.emit(Route::Room(id.clone()));
        })
    };
    let key_activate = {
        let on_navigate = p.on_navigate.clone();
        let on_swipe = p.on_swipe.clone();
        let id = id.clone();
        let track = track.clone();
        let actions = actions.clone();
        let close_drawer = close_drawer.clone();
        Callback::from(move |e: KeyboardEvent| match e.key().as_str() {
            "Enter" | " " => {
                e.prevent_default();
                e.stop_propagation();
                if is_open {
                    close_drawer();
                } else {
                    on_navigate.emit(Route::Room(id.clone()));
                }
            }
            // The drawer without a touchscreen. A keyboard cannot swipe, and
            // an action reachable only by gesture is an action some people
            // simply do not have.
            "Delete" | "Backspace" if !is_open => {
                e.prevent_default();
                swipe::slide(&track, -swipe::width(&actions), true);
                on_swipe.emit(Some(id.clone()));
            }
            "Escape" if is_open => {
                e.stop_propagation();
                close_drawer();
            }
            _ => {}
        })
    };

    // --- the gesture ------------------------------------------------------
    let onpointerdown = {
        let drag = drag.clone();
        let track = track.clone();
        let actions = actions.clone();
        Callback::from(move |e: PointerEvent| {
            let mut d = drag.borrow_mut();
            d.start(&e, swipe::width(&actions), is_open);
            swipe::set_dragging(&track, true);
        })
    };

    let onpointermove = {
        let drag = drag.clone();
        let track = track.clone();
        Callback::from(move |e: PointerEvent| {
            let mut d = drag.borrow_mut();
            let Some(offset) = d.mv(&e) else {
                return;
            };
            swipe::slide(&track, offset, false);
        })
    };

    let end = {
        let drag = drag.clone();
        let track = track.clone();
        let on_swipe = p.on_swipe.clone();
        let store = store.clone();
        let room = r.clone();
        let id = id.clone();
        Callback::from(move |e: PointerEvent| {
            let outcome = drag.borrow_mut().end(&e);
            swipe::set_dragging(&track, false);
            match outcome {
                swipe::Outcome::Idle => {}
                swipe::Outcome::Closed => {
                    swipe::slide(&track, 0.0, true);
                    on_swipe.emit(None);
                }
                swipe::Outcome::Opened(w) => {
                    swipe::slide(&track, -w, true);
                    on_swipe.emit(Some(id.clone()));
                }
                // The learned shortcut: past the drawer entirely and the
                // confirmation is the next thing on screen. The row springs
                // back first — the dialog, not a half-swiped row, is what is
                // being answered.
                swipe::Outcome::Express => {
                    swipe::slide(&track, 0.0, true);
                    on_swipe.emit(None);
                    swipe::confirm_hide(&store, &room);
                }
            }
        })
    };

    // Rotation pending replaces the preview: "the last thing said" matters less
    // than "you cannot post here until someone re-keys".
    let preview = if rotating {
        "Key rotation needed".to_owned()
    } else {
        r.last_message
            .as_ref()
            .filter(|m| m.kind().is_renderable())
            .map(|m| {
                if m.is_encrypted {
                    // Never render ciphertext as a preview. The room list has
                    // no key material and must not pretend otherwise.
                    "🔒 Encrypted message".to_owned()
                } else {
                    format::preview(&m.content, 60)
                }
            })
            .unwrap_or_default()
    };

    let time = r
        .last_message
        .as_ref()
        .map(|m| format::room_list_time(m.message_timestamp, p.now, p.tz));

    let first_letter = r
        .room
        .name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string());

    html! {
        <div
            class="fn-swipe"
            data-open={is_open.then_some("true")}
            {onpointerdown}
            {onpointermove}
            onpointerup={end.clone()}
            onpointercancel={end}
        >
            <div ref={track} class="fn-swipe__track">
                <div
                    {class}
                    // `--row-art` is the room's own sigil (the same file the
                    // <Ident> shows), worn as a masked backdrop by app.css §6
                    // — the rack treatment. Passed as a variable rather than
                    // styled here so the stylesheet keeps deciding *whether*
                    // imagery appears at all.
                    style={format!(
                        "--i: {}; --row-art: url('/static/img/{}.png')",
                        p.index,
                        crate::identity::art_for(id.as_str()),
                    )}
                    role="option"
                    aria-selected={selected.to_string()}
                    tabindex={if p.tabbable { "0" } else { "-1" }}
                    data-name={r.room.name.clone()}
                    onclick={activate}
                    onkeydown={key_activate}
                >
                    <Ident
                        seed={id.to_string()}
                        class="fn-room-row__avatar"
                        corner={first_letter}
                        zoom={crate::components::common::Zoom {
                            title: r.room.name.clone(),
                            subtitle: None,
                            copy: None,
                        }}
                    />
                    <div class="fn-room-row__title">
                        <span class="fn-room-row__name">{ &r.room.name }</span>
                        if r.has_encryption {
                            <Lock pending={rotating} />
                        }
                    </div>
                    <div class="fn-room-row__meta">
                        if is_admin {
                            <Badge variant="admin">{ t(lang, Key::admin) }</Badge>
                        }
                        <span>{ t(lang, if r.member_count == 1 {
                                    Key::member_count_one
                                } else {
                                    Key::member_count_many
                                }).replace("{n}", &r.member_count.to_string()) }</span>
                        if !preview.is_empty() {
                            <span class="fn-room-row__preview">{ preview }</span>
                        }
                    </div>
                    <div class="fn-room-row__aside">
                        if let Some(t) = time {
                            <span class="fn-room-row__time">{ t }</span>
                        }
                        <Unread count={unread} />
                    </div>
                </div>
                { drawer(lang, &store, r, is_open, actions) }
            </div>
        </div>
    }
}

/// What the swipe reveals: hide, and leave.
///
/// Deleting the room for everybody is *not* here. It is the one room action
/// that reaches other people's clients, and a gesture is the wrong amount of
/// deliberation for it — it stays in the `⋮` menu, behind admin rights, where
/// you have to have gone looking.
///
/// The buttons are rendered even while the drawer is shut, so the reveal is a
/// transform and nothing else: mounting two buttons mid-gesture is a layout
/// pass in the middle of a finger movement. They leave the tab order and the
/// accessibility tree while they are out of sight.
fn drawer(lang: Lang, store: &Store, r: &RoomWithMembers, is_open: bool, actions: NodeRef) -> Html {
    let action = |label: String, icon: Html, danger: bool, c: Confirm| {
        let store = store.clone();
        let onclick = Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            swipe::mark_intent();
            store.dispatch(Action::OpenModal(Modal::Confirm(c.clone())));
        });
        html! {
            <button
                type="button"
                class={classes!("fn-swipe__action", danger.then_some("fn-swipe__action--danger"))}
                tabindex={if is_open { "0" } else { "-1" }}
                aria-hidden={(!is_open).then_some("true")}
                {onclick}
            >
                { icon }
                <span>{ label }</span>
            </button>
        }
    };

    html! {
        <div
            ref={actions}
            class="fn-swipe__actions"
            role="group"
            aria-label={t(lang, Key::swipe_actions_for).replace("{name}", &r.room.name)}
        >
            { action(t(lang, Key::hide).to_owned(), icons::eye_off(18), false,
                     swipe::hide_confirm(lang, r)) }
            { action(t(lang, Key::leave).to_owned(), icons::power(18), true, Confirm {
                title: t(lang, Key::leave_room_title).replace("{name}", &r.room.name),
                body: t(lang, Key::leave_room_body).into(),
                confirm_label: t(lang, Key::leave_room).into(),
                action: ConfirmAction::LeaveRoom(r.id().clone()),
            }) }
        </div>
    }
}
