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
use crate::state::{use_store, Action, KnowledgeSeed, Load, Modal, Store};

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

                { body(lang, &store, &rooms, p, &needle, now, tz, list_ref, onkeydown) }
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
                        // Roving tabindex: the selected row, or the first.
                        let tabbable = selected || (p.selected.is_none() && i == 0);
                        // `--i` drives the CSS entrance stagger (app.css §14);
                        // the delay is capped there, not here.
                        row(lang, r, i, selected, tabbable, unread, me.as_ref(), now, tz, &p.on_navigate)
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

#[allow(clippy::too_many_arguments)]
fn row(
    lang: Lang,
    r: &RoomWithMembers,
    index: usize,
    selected: bool,
    tabbable: bool,
    unread: u32,
    me: Option<&pocketskynet_core::WalletAddress>,
    now: i64,
    tz: i32,
    on_navigate: &Callback<Route>,
) -> Html {
    let id = r.id().clone();
    let is_admin = me.is_some_and(|m| r.is_admin(m));
    let rotating = r.room.key_rotation_pending;

    let mut class = classes!("fn-room-row");
    if unread > 0 {
        class.push("is-unread");
    }
    if rotating {
        class.push("is-rotation-pending");
    }

    let activate = {
        let on_navigate = on_navigate.clone();
        let id = id.clone();
        Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Room(id.clone())))
    };
    let key_activate = {
        let on_navigate = on_navigate.clone();
        let id = id.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" || e.key() == " " {
                e.prevent_default();
                e.stop_propagation();
                on_navigate.emit(Route::Room(id.clone()));
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
        .map(|m| format::room_list_time(m.message_timestamp, now, tz));

    let first_letter = r
        .room
        .name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string());

    html! {
        <div
            key={id.to_string()}
            {class}
            // `--row-art` is the room's own sigil (the same file the <Ident>
            // shows), worn as a masked backdrop by app.css §6 — the rack
            // treatment. Passed as a variable rather than styled here so the
            // stylesheet keeps deciding *whether* imagery appears at all.
            style={format!(
                "--i: {index}; --row-art: url('/static/img/{}.png')",
                crate::identity::art_for(id.as_str()),
            )}
            role="option"
            aria-selected={selected.to_string()}
            tabindex={if tabbable { "0" } else { "-1" }}
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
    }
}
