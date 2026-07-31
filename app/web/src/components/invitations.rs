//! Screen 7 — Invitations inbox (DESIGN.md §11).
//!
//! Consent gets its own screen. An invitation creates nothing until it is
//! accepted, so it cannot live only in a toast that can be missed — hence both
//! surfaces: a pinned row at the top of the room list, and this pane.
//!
//! Decline is `topcoat-button`, **not** red: declining an invitation is not
//! destruction, and colouring it as such would train people to hesitate over
//! the safe option.

use pocketskynet_core::RoomId;
use yew::prelude::*;

use crate::actions;
use crate::format;
use crate::route::Route;
use crate::state::{use_store, Load};

use super::common::{Addr, Back, BusyButton, Empty, Ident, IdentSize, Skeleton};
use super::toast;
use crate::i18n::{t, Key};

#[derive(Properties, PartialEq)]
pub struct InvitationsProps {
    pub on_navigate: Callback<Route>,
}

#[function_component(Invitations)]
pub fn invitations(p: &InvitationsProps) -> Html {
    let store = use_store();
    let lang = store.language;
    // Only the card being acted on disables; the rest stay live, because
    // accepting one invitation says nothing about the others.
    let acting = use_state(|| Option::<RoomId>::None);
    let now = format::now_ms();
    let offline = !store.online;

    let act = {
        let store = store.clone();
        let acting = acting.clone();
        let on_navigate = p.on_navigate.clone();
        Callback::from(move |(room_id, accept, name): (RoomId, bool, String)| {
            if acting.is_some() {
                return;
            }
            acting.set(Some(room_id.clone()));
            let store = store.clone();
            let acting = acting.clone();
            let on_navigate = on_navigate.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = if accept {
                    store.client.accept_invitation(&room_id).await
                } else {
                    store.client.decline_invitation(&room_id).await
                };
                match result {
                    Ok(()) if accept => {
                        toast::success(&store, t(lang, Key::joined_room).replace("{name}", &name));
                        actions::refresh_rooms(store.clone()).await;
                        actions::refresh_invitations(store.clone()).await;
                        on_navigate.emit(Route::Room(room_id));
                    }
                    Ok(()) => {
                        toast::neutral(&store, "Invitation declined");
                        actions::refresh_invitations(store.clone()).await;
                    }
                    Err(e) => {
                        // A 404 means the invitation is stale — the room was
                        // deleted, or you were added another way. Refresh
                        // rather than leaving a card that can never succeed.
                        if e.is_not_found() {
                            toast::warn(&store, t(lang, Key::invitation_gone), None);
                            actions::refresh_invitations(store.clone()).await;
                        } else {
                            toast::error(
                                &store,
                                "Couldn't respond to the invitation",
                                Some(e.user_message()),
                            );
                        }
                    }
                }
                acting.set(None);
            });
        })
    };

    html! {
        <>
            <div class="topcoat-navigation-bar">
                <Back onclick={{
                    let on_navigate = p.on_navigate.clone();
                    Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Rooms))
                }} />
                <h1 class="topcoat-navigation-bar__title">{ t(lang, Key::invitations) }</h1>
                <span class="fn-badge fn-badge--muted">{ store.invitations.len() }</span>
            </div>

            if offline {
                <super::common::OfflineBanner />
            }

            <div class="fn-scroll" style="padding: var(--fn-s4)">
                { match (&store.invitations_load, store.invitations.is_empty()) {
                    (Load::Loading, true) => html! { <Skeleton rows={2} /> },
                    (Load::Error(e), true) => html! {
                        <Empty art="⚠️" title={t(lang, Key::couldnt_load_invitations)} art_class="fn-art--offline"
                               description={e.clone()} is_error=true />
                    },
                    (_, true) => html! {
                        <Empty
                            art="✉️"
                            title={t(lang, Key::no_invitations)}
                            art_class="fn-art--invitations"
                            description={t(lang, Key::invitations_empty_body)}
                        />
                    },
                    _ => html! {
                        <ul class="fn-stack" style="list-style:none;padding:0;margin:0">
                            { for store.invitations.iter().enumerate().map(|(i, inv)| {
                                let busy = acting.as_ref() == Some(&inv.room_id);
                                let disabled = acting.is_some() || offline;
                                html! {
                                    <li key={inv.room_id.to_string()} class="fn-picklist__row"
                                        style={format!("--i: {i}")}>
                                        <Ident seed={inv.room_id.to_string()} size={IdentSize::Lg} />
                                        <div class="fn-grow">
                                            <strong>{ &inv.room_name }</strong>
                                            <div class="fn-muted">
                                                { t(lang, Key::invitation_from).replace("{name}", &inv.inviter_name()) }
                                                <Addr address={inv.invited_by.clone()} />
                                                { format!(" · {}", inv.created_at.as_deref()
                                                    .and_then(format::parse_iso8601_ms)
                                                    .map(|t| format::relative_time(t, now))
                                                    .unwrap_or_else(|| "recently".into())) }
                                            </div>
                                            <p class="fn-field__help">
                                                { t(lang, Key::invite_key_note) }
                                            </p>
                                        </div>
                                        <div class="fn-row">
                                            <button
                                                type="button"
                                                class="topcoat-button"
                                                disabled={disabled}
                                                onclick={{
                                                    let act = act.clone();
                                                    let id = inv.room_id.clone();
                                                    let name = inv.room_name.clone();
                                                    Callback::from(move |_: MouseEvent| {
                                                        act.emit((id.clone(), false, name.clone()))
                                                    })
                                                }}
                                            >{ t(lang, Key::decline) }</button>
                                            <BusyButton
                                                label={t(lang, Key::accept)}
                                                class="topcoat-button--cta"
                                                busy={busy}
                                                disabled={disabled}
                                                onclick={{
                                                    let act = act.clone();
                                                    let id = inv.room_id.clone();
                                                    let name = inv.room_name.clone();
                                                    Callback::from(move |_: MouseEvent| {
                                                        act.emit((id.clone(), true, name.clone()))
                                                    })
                                                }}
                                            />
                                        </div>
                                    </li>
                                }
                            }) }
                        </ul>
                    },
                } }
            </div>
        </>
    }
}
