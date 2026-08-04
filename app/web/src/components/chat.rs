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
    Back, Badge, Banner, BusyButton, ConnPill, Empty, Ident, IdentSize, Lock, Popover, Spinner,
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

    // Keep the stream pinned to the bottom as messages arrive. Anchoring on a
    // revision rather than on a length means edits and reactions also settle
    // the scroll, and history loads (which prepend) do not.
    {
        let stream_ref = stream_ref.clone();
        let count = state.messages.len();
        use_effect_with(count, move |_| {
            if let Some(el) = stream_ref.cast::<HtmlElement>() {
                el.set_scroll_top(el.scroll_height());
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

    let is_admin = room.is_admin(&me);
    let rotation_pending = room.room.key_rotation_pending;
    // Carries *why* an encrypted post cannot succeed, not merely that it
    // cannot: the composer's placeholder is the only explanation the user gets,
    // and the remedies differ.
    let composer_blocked = store.post_block(&p.room_id);
    let offline = !store.online;
    let bundle = store.bundle(&p.room_id).cloned();
    let now = format::now_ms();
    let tz = format::tz_offset_minutes();

    // --- callbacks -------------------------------------------------------

    let on_send = {
        let store = store.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |text: String| {
            let now = format::now_ms();
            let local_id = crate::state::next_local_id();
            store.dispatch(Action::QueueSend(
                room_id.clone(),
                local_id,
                text.clone(),
                now,
            ));
            let store2 = store.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                actions::send_message(store2, room_id, local_id, text).await;
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
            if super::common::copy_to_clipboard(&text) {
                toast::success(&store, t(lang, Key::copied));
            } else {
                toast::error(
                    &store,
                    t(lang, Key::couldnt_copy),
                    Some(t(lang, Key::clipboard_blocked).into()),
                );
            }
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

    let visible: Vec<&Message> = state.ordered(&store.blocks);
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
                    zoom={crate::components::common::Zoom {
                        title: room.room.name.clone(),
                        subtitle: None,
                        copy: None,
                    }}
                />
                <div class="fn-chat__title fn-grow">
                    <span>{ &room.room.name }</span>
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
            { room_menu(lang, &store, &room, is_admin, *menu_open, menu_open.clone(), &p.on_navigate) }

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

            <div
                ref={stream_ref}
                class="fn-stream fn-scroll"
                role="log"
                aria-live="polite"
                aria-relevant="additions"
                aria-label={t(lang, Key::messages_in_room).replace("{room}", &room.room.name)}
            >
                if state.has_more_history && !visible.is_empty() {
                    <button
                        type="button"
                        class="topcoat-button--quiet"
                        disabled={*loading_older}
                        onclick={on_load_older}
                    >
                        { if *loading_older { t(lang, Key::loading) } else { t(lang, Key::load_earlier) } }
                    </button>
                }

                if visible.is_empty() && pending.is_empty() {
                    { empty_stream(lang, &load, room.has_encryption) }
                }

                { for visible.iter().enumerate().map(|(i, m)| {
                    let prev: Option<&Message> = (i > 0).then(|| visible[i - 1]);
                    let body = match &bundle {
                        Some(b) => decrypt_message(b, &p.room_id, m),
                        None if m.is_encrypted => Decrypted::NoKeyForEpoch(m.key_version()),
                        None => Decrypted::Plaintext(m.content.clone()),
                    };
                    html! {
                        <>
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
                                on_knowledge={{
                                    let store = store.clone();
                                    let on_navigate = p.on_navigate.clone();
                                    Callback::from(move |seed: crate::state::KnowledgeSeed| {
                                        store.dispatch(Action::SeedKnowledge(seed));
                                        on_navigate.emit(Route::Knowledge);
                                    })
                                }}
                            />
                        </>
                    }
                }) }

                { for pending.iter().map(|q| pending_bubble(lang, q, &store, &p.room_id)) }
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
                room_name={room.room.name.clone()}
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
                    Callback::from(move |(name, bytes, caption): (String, Vec<u8>, String)| {
                        let store = store.clone();
                        let id = id.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            crate::actions::attach_file(store, id, name, bytes, caption).await;
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
                actions::send_message(store, room_id, local_id, text).await;
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
    is_admin: bool,
    is_open: bool,
    open: UseStateHandle<bool>,
    on_navigate: &Callback<Route>,
) -> Html {
    let id = room.id().clone();
    let name = room.room.name.clone();

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
            if is_admin || room.has_encryption {
                { item(t(lang, Key::invite_people), Modal::Invite(id.clone()), open.clone()) }
            }
            if is_admin {
                { item(t(lang, Key::rename_room), Modal::RenameRoom(id.clone(), name.clone()), open.clone()) }
                { item(t(lang, Key::manage_admins), Modal::ManageAdmins(id.clone()), open.clone()) }
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

            { confirm(t(lang, Key::leave_room), false,
                      t(lang, Key::leave_room_title).replace("{name}", &name),
                      t(lang, Key::leave_room_body).to_owned(),
                      t(lang, Key::leave_room).to_owned(),
                      ConfirmAction::LeaveRoom(id.clone()), open.clone()) }
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
