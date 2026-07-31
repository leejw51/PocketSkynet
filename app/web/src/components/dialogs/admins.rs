//! Manage admins (DESIGN.md §10). Min 1, max 9, server-enforced.

use pocketskynet_core::{RoomId, WalletAddress};
use yew::prelude::*;

use crate::actions;
use crate::state::{use_store, Action, Confirm, ConfirmAction, Modal};

use super::super::common::{Addr, BusyButton, Ident, IdentSize};
use super::super::modal::Modal as Dialog;
use crate::i18n::{t, Key};

/// Manage admins (DESIGN.md §10). Min 1, max 9, server-enforced.
#[derive(Properties, PartialEq)]
pub struct AdminsProps {
    pub room_id: RoomId,
    pub on_close: Callback<()>,
}

#[function_component(ManageAdmins)]
pub fn manage_admins(p: &AdminsProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let busy = use_state(|| Option::<WalletAddress>::None);
    let error = use_state(|| Option::<String>::None);

    let Some(room) = store.room(&p.room_id).cloned() else {
        return html! {};
    };
    let Some(me) = store.me().cloned() else {
        return html! {};
    };
    let admins = room.admins.clone();
    let full = admins.len() >= 9;
    let last_one = admins.len() <= 1;

    let mutate = {
        let store = store.clone();
        let busy = busy.clone();
        let error = error.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |(who, promote): (WalletAddress, bool)| {
            if busy.is_some() {
                return;
            }
            busy.set(Some(who.clone()));
            error.set(None);
            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = if promote {
                    store.client.add_admin(&room_id, &who).await
                } else {
                    store.client.remove_admin(&room_id, &who).await
                };
                match result {
                    Ok(()) => actions::refresh_rooms(store.clone()).await,
                    Err(e) => error.set(Some(e.user_message())),
                }
                busy.set(None);
            });
        })
    };

    let close = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: ()| on_close.emit(()))
    };
    let close_click = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: MouseEvent| on_close.emit(()))
    };

    let candidates: Vec<&crate::api::RoomMember> = room
        .members
        .iter()
        .filter(|m| {
            m.user_address != me && !admins.iter().any(|a| a.wallet_address == m.user_address)
        })
        .collect();

    html! {
        <Dialog
            title={t(lang, Key::manage_admins)}
            description={t(lang, Key::admins_can_note)}
            wide=true
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button--cta" onclick={close_click}>{ t(lang, Key::done) }</button>
            })}
        >
            <div class="fn-row">
                <span class="fn-grow">{ t(lang, Key::current_admins) }</span>
                <span class="fn-admin-count fn-nums" data-full={full.to_string()}>
                    { format!("{} / 9", admins.len()) }
                </span>
            </div>

            if let Some(e) = &*error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }

            { for admins.iter().enumerate().map(|(i, a)| {
                let is_me = a.wallet_address == me;
                let acting = busy.as_ref() == Some(&a.wallet_address);
                html! {
                    <div key={a.wallet_address.to_string()} class="fn-admin-card"
                         style={format!("--i: {i}")}>
                        <Ident seed={a.wallet_address.to_string()} size={IdentSize::Xs} is_self={is_me} image={a.profile_image.clone()} />
                        <span class="fn-grow">{ a.display_name() }</span>
                        <Addr address={a.wallet_address.clone()} />
                        if is_me {
                            <span class="fn-badge fn-badge--self">{ t(lang, Key::you) }</span>
                        }
                        <BusyButton
                            label={t(lang, Key::remove)}
                            class="topcoat-button"
                            busy={acting}
                            // A room with no admin can never be managed again.
                            disabled={last_one || busy.is_some()}
                            onclick={{
                                let mutate = mutate.clone();
                                let store = store.clone();
                                let room_id = p.room_id.clone();
                                let who = a.wallet_address.clone();
                                Callback::from(move |_: MouseEvent| {
                                    if is_me {
                                        // Giving up your own admin rights is
                                        // one-way unless someone re-promotes
                                        // you, so it is confirmed.
                                        store.dispatch(Action::OpenModal(Modal::Confirm(Confirm {
                                            title: t(lang, Key::give_up_admin_title).into(),
                                            body: t(lang, Key::give_up_admin_body).into(),
                                            confirm_label: t(lang, Key::give_up_admin).into(),
                                            action: ConfirmAction::RemoveAdmin(
                                                room_id.clone(), who.clone()),
                                        })));
                                    } else {
                                        mutate.emit((who.clone(), false));
                                    }
                                })
                            }}
                        />
                    </div>
                }
            }) }
            if last_one {
                <p class="fn-field__help">{ t(lang, Key::need_one_admin) }</p>
            }

            <h3>{ t(lang, Key::add_an_admin) }</h3>
            if full {
                <p class="fn-field__error">
                    { t(lang, Key::admin_limit_reached) }
                </p>
            } else if candidates.is_empty() {
                <p class="fn-muted">{ t(lang, Key::everyone_is_admin) }</p>
            } else {
                <div class="fn-picklist">
                    { for candidates.iter().enumerate().map(|(i, m)| {
                        let acting = busy.as_ref() == Some(&m.user_address);
                        html! {
                            <div key={m.user_address.to_string()} class="fn-picklist__row"
                                 style={format!("--i: {i}")}>
                                <Ident seed={m.user_address.to_string()} size={IdentSize::Xs} image={m.user.profile_image.clone()} />
                                <span class="fn-grow">{ m.user.display_name() }</span>
                                <Addr address={m.user_address.clone()} />
                                <BusyButton
                                    label={t(lang, Key::make_admin)}
                                    class="topcoat-button"
                                    busy={acting}
                                    disabled={busy.is_some()}
                                    onclick={{
                                        let mutate = mutate.clone();
                                        let who = m.user_address.clone();
                                        Callback::from(move |_: MouseEvent| mutate.emit((who.clone(), true)))
                                    }}
                                />
                            </div>
                        }
                    }) }
                </div>
            }
        </Dialog>
    }
}
