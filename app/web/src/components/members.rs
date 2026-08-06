//! Screen 6 — Members & admins (DESIGN.md §10).
//!
//! Blocked members are **marked, not hidden**. Hiding them would leave a
//! blocker wondering why a conversation has gaps; the badge explains it. The
//! roster itself is not block-filtered server-side, which is what makes this
//! possible.

use pocketskynet_core::{RoomId, WalletAddress};
use yew::prelude::*;

use crate::api::RoomMember;
use crate::route::Route;
use crate::state::{use_store, Action, Confirm, ConfirmAction, Load, Modal};

use super::common::{
    copy_with_toast, hit_control, Addr, Back, Badge, Empty, IconButton, Ident, IdentSize,
    PresenceLabel, Skeleton,
};
use super::icons;
use crate::i18n::{t, Key, Lang};

#[derive(Properties, PartialEq)]
pub struct MembersProps {
    pub room_id: RoomId,
    pub on_navigate: Callback<Route>,
}

#[function_component(Members)]
pub fn members(p: &MembersProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let roster = use_state(Vec::<RoomMember>::new);
    let load = use_state(Load::default);

    // Fetch the roster whenever the room changes. `GET /api/rooms/:id/members`
    // rather than the nested `members` from the room list, because the latter
    // can be stale by a whole membership change.
    {
        let store = store.clone();
        let roster = roster.clone();
        let load = load.clone();
        let room_id = p.room_id.clone();
        use_effect_with(room_id.clone(), move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.members(&room_id).await {
                    Ok(v) => {
                        roster.set(v);
                        load.set(Load::Ready);
                    }
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
            || ()
        });
    }

    let Some(me) = store.me().cloned() else {
        return html! {};
    };
    let room = store.room(&p.room_id).cloned();
    let is_admin = room.as_ref().is_some_and(|r| r.is_admin(&me));
    let admins: Vec<WalletAddress> = room
        .as_ref()
        .map(|r| r.admins.iter().map(r_addr).collect())
        .unwrap_or_default();
    let offline = !store.online;

    let confirm = {
        let store = store.clone();
        Callback::from(move |c: Confirm| store.dispatch(Action::OpenModal(Modal::Confirm(c))))
    };

    html! {
        <>
            <div class="topcoat-navigation-bar">
                <Back onclick={{
                    let on_navigate = p.on_navigate.clone();
                    let id = p.room_id.clone();
                    Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Room(id.clone())))
                }} />
                <h1 class="topcoat-navigation-bar__title">{ t(lang, Key::nav_members) }</h1>
                <span class="fn-badge fn-badge--muted">{ roster.len() }</span>
                <span class="fn-push">
                    if is_admin {
                        <button
                            type="button"
                            class="topcoat-button--cta"
                            disabled={offline}
                            onclick={{
                                let store = store.clone();
                                let id = p.room_id.clone();
                                Callback::from(move |_: MouseEvent| {
                                    store.dispatch(Action::OpenModal(Modal::Invite(id.clone())))
                                })
                            }}
                        >{ t(lang, Key::invite_people) }</button>
                    }
                </span>
            </div>

            if offline {
                <super::common::OfflineBanner />
            }

            <div class="fn-people fn-scroll">
                { match &*load {
                    Load::Loading if roster.is_empty() => html! { <Skeleton rows={5} /> },
                    Load::Error(e) => html! {
                        <Empty art="⚠️" title={t(lang, Key::couldnt_load_members)} art_class="fn-art--offline"
                               description={e.clone()} is_error=true />
                    },
                    _ => html! {
                        <ul class="fn-stack" style="list-style:none;padding:0;margin:0">
                            { for roster.iter().enumerate().map(|(i, m)| person_row(lang,
                                m, i, &me, &admins, is_admin, offline, &p.room_id,
                                &store, &confirm,
                            )) }
                        </ul>
                    },
                } }
            </div>
        </>
    }
}

fn r_addr(u: &crate::api::User) -> WalletAddress {
    u.wallet_address.clone()
}

#[allow(clippy::too_many_arguments)]
fn person_row(
    lang: Lang,
    m: &RoomMember,
    index: usize,
    me: &WalletAddress,
    admins: &[WalletAddress],
    viewer_is_admin: bool,
    offline: bool,
    room_id: &RoomId,
    store: &crate::state::Store,
    confirm: &Callback<Confirm>,
) -> Html {
    let is_me = &m.user_address == me;
    let is_admin = admins.contains(&m.user_address);
    let blocked = store.blocks.hides(&m.user_address);
    let presence = store.presence_of(&m.user_address);
    let name = m.user.display_name();

    let mut class = classes!("fn-person", "fn-person--tap");
    if is_me {
        class.push("fn-person--self");
    }

    // Tapping the card copies the address. A wallet address is the only thing
    // a member row is ever *for* outside of blocking someone — you look
    // someone up here in order to send them funds, or to paste them into an
    // invite — and forty-two mono characters is not a thing anyone selects by
    // hand on a phone. The address itself is also a button (`<Addr>`), which
    // is what gives the keyboard and a screen reader a named target; this is
    // the pointer's larger, unnamed version of it.
    let copy_card = {
        let store = store.clone();
        let who = m.user_address.clone();
        Callback::from(move |e: MouseEvent| {
            if hit_control(&e) {
                return;
            }
            copy_with_toast(&store, &who.to_checksummed(), t(lang, Key::address_copied));
        })
    };

    // Never render actions on your own row: "block yourself" and "remove
    // yourself" are either impossible or have a dedicated flow (Leave room).
    let actions = if is_me {
        html! {}
    } else {
        html! {
            <div class="fn-person__actions">
                <IconButton
                    label={if blocked {
                        t(lang, Key::unblock_person).replace("{name}", &name)
                    } else {
                        t(lang, Key::block_person).replace("{name}", &name)
                    }}
                    icon={icons::ban(16)}
                    disabled={offline}
                    onclick={{
                        let confirm = confirm.clone();
                        let who = m.user_address.clone();
                        let name = name.clone();
                        Callback::from(move |_: MouseEvent| confirm.emit(if blocked {
                            Confirm {
                                title: t(lang, Key::unblock_title).replace("{name}", &name),
                                body: t(lang, Key::unblock_body).into(),
                                confirm_label: t(lang, Key::unblock).into(),
                                action: ConfirmAction::UnblockUser(who.clone()),
                            }
                        } else {
                            Confirm {
                                title: t(lang, Key::block_title).replace("{name}", &name),
                                body: t(lang, Key::block_body).into(),
                                confirm_label: t(lang, Key::block).into(),
                                action: ConfirmAction::BlockUser(who.clone()),
                            }
                        }))
                    }}
                />
                if viewer_is_admin {
                    <IconButton
                        label={t(lang, Key::remove_from_room).replace("{name}", &name)}
                        icon={icons::minus_circle(16)}
                        disabled={offline}
                        onclick={{
                            let confirm = confirm.clone();
                            let who = m.user_address.clone();
                            let room_id = room_id.clone();
                            let name = name.clone();
                            Callback::from(move |_: MouseEvent| confirm.emit(Confirm {
                                title: t(lang, Key::remove_member_title).replace("{name}", &name),
                                body: t(lang, Key::remove_member_body).into(),
                                confirm_label: t(lang, Key::remove).into(),
                                action: ConfirmAction::KickMember(room_id.clone(), who.clone()),
                            }))
                        }}
                    />
                    <IconButton
                        label={t(lang, Key::manage_admins_label).to_owned()}
                        icon={icons::crown(16)}
                        disabled={offline}
                        onclick={{
                            let store = store.clone();
                            let room_id = room_id.clone();
                            Callback::from(move |_: MouseEvent| {
                                store.dispatch(Action::OpenModal(Modal::ManageAdmins(room_id.clone())))
                            })
                        }}
                    />
                }
            </div>
        }
    };

    html! {
        <li
            key={m.user_address.to_string()}
            {class}
            style={format!("--i: {index}")}
            title={t(lang, Key::tap_to_copy_address)}
            onclick={copy_card}
        >
            <Ident
                seed={m.user_address.to_string()}
                size={IdentSize::Lg}
                is_self={is_me}
                presence={presence}
                image={m.user.profile_image.clone()}
                zoom={crate::components::common::Zoom {
                    title: m.user.username.clone(),
                    subtitle: None,
                    address: Some(m.user_address.clone()),
                }}
            />
            <div class="fn-grow">
                <div class="fn-person__name">
                    <span>{ &name }</span>
                    if is_admin {
                        <span class="fn-crown-icon" role="img" aria-label={t(lang, Key::admin)} title={t(lang, Key::admin)}></span>
                        <Badge variant="admin">{ t(lang, Key::admin) }</Badge>
                    }
                    if is_me {
                        <Badge variant="self">{ t(lang, Key::you) }</Badge>
                    }
                    if blocked {
                        <Badge variant="danger">{ t(lang, Key::blocked) }</Badge>
                    }
                </div>
                <div class="fn-person__meta">
                    // The word, not just the dot: the tile is `aria-hidden`
                    // decoration, and a colour on its own is never the whole
                    // signal (DESIGN.md §17). This is the roster — the one
                    // screen with room to spell it out.
                    <PresenceLabel status={presence} />
                    <Addr address={m.user_address.clone()} />
                </div>
            </div>
            { actions }
        </li>
    }
}
