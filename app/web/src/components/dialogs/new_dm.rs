//! Start a direct message.
//!
//! Deliberately the same picker as Invite, because it is the same question —
//! "which of these people do you mean" — and answering it two different ways
//! in two dialogs is how a product ends up with two search behaviours, two
//! empty states and one of them subtly wrong.
//!
//! What differs is what happens on selection. Inviting is a request the other
//! person accepts; opening a DM is not. `POST /api/rooms/dm` is idempotent on
//! the member set, so pressing this twice — or pressing it while the other
//! person presses it from their side — lands in one room rather than two.
//! That is why this dialog navigates rather than reporting "sent".

use pocketskynet_core::WalletAddress;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::api::User;
use crate::route::Route;
use crate::state::{use_store, Load};

use super::super::common::{Addr, Empty, Ident, IdentSize, Skeleton};
use super::super::modal::Modal as Dialog;
use crate::i18n::{t, Key};

#[derive(Properties, PartialEq)]
pub struct NewDirectMessageProps {
    pub on_close: Callback<()>,
    pub on_navigate: Callback<Route>,
}

#[function_component(NewDirectMessage)]
pub fn new_direct_message(p: &NewDirectMessageProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let query = use_state(String::new);
    let results = use_state(Vec::<User>::new);
    let load = use_state(Load::default);
    let opening = use_state(|| Option::<WalletAddress>::None);
    let error = use_state(|| Option::<String>::None);

    // Debounced exactly as Invite is, and for the same reason: typing a full
    // address should not be ten requests against a 100/min limiter.
    {
        let store = store.clone();
        let results = results.clone();
        let load = load.clone();
        let q = (*query).clone();
        use_effect_with(q.clone(), move |_| {
            let trimmed = q.trim().to_owned();
            if trimmed.len() < 2 {
                results.set(Vec::new());
                load.set(Load::Idle);
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            load.set(Load::Loading);
            let handle = gloo_timers::callback::Timeout::new(250, move || {
                wasm_bindgen_futures::spawn_local(async move {
                    match store.client.search_users(&trimmed).await {
                        Ok(v) => {
                            results.set(v);
                            load.set(Load::Ready);
                        }
                        Err(e) => load.set(Load::Error(e.user_message())),
                    }
                });
            });
            Box::new(move || drop(handle))
        });
    }

    let open = {
        let store = store.clone();
        let opening = opening.clone();
        let error = error.clone();
        let on_close = p.on_close.clone();
        let on_navigate = p.on_navigate.clone();
        Callback::from(move |who: WalletAddress| {
            let store = store.clone();
            let opening = opening.clone();
            let error = error.clone();
            let on_close = on_close.clone();
            let on_navigate = on_navigate.clone();
            opening.set(Some(who.clone()));
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.open_dm(std::slice::from_ref(&who)).await {
                    Ok(room) => {
                        let id = room.id().clone();
                        // Refresh before navigating: the room list is what the
                        // conversation screen reads its roster from, and a DM
                        // with no roster has no title.
                        crate::actions::refresh_rooms(store.clone()).await;
                        on_close.emit(());
                        on_navigate.emit(Route::Room(id));
                    }
                    Err(e) if e.is_not_found() => {
                        opening.set(None);
                        // The server refuses a DM to a wallet that has never
                        // signed in, because the roster could not render it.
                        error.set(Some(
                            "That wallet hasn't signed in to this server yet. Invite them to a \
                             room instead."
                                .into(),
                        ));
                    }
                    Err(e) => {
                        opening.set(None);
                        error.set(Some(e.user_message()));
                    }
                }
            });
        })
    };

    // The note-to-self shortcut. A DM whose only member is you is the same
    // mechanism with a one-element set, and it is genuinely useful — it is
    // where people park links for themselves — so it gets a row rather than
    // being a trick you have to know.
    let open_self = {
        let open = open.clone();
        let me = store.me().cloned();
        Callback::from(move |_: MouseEvent| {
            if let Some(me) = me.clone() {
                open.emit(me);
            }
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

    let me = store.me().cloned();

    html! {
        <Dialog
            title={t(lang, Key::new_direct_message)}
            description={t(lang, Key::new_direct_message_hint)}
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button" onclick={close_click}>
                    { t(lang, Key::cancel) }
                </button>
            })}
        >
            <input
                data-autofocus="true"
                class="topcoat-search-input"
                type="search"
                placeholder={t(lang, Key::username_or_address)}
                aria-label={t(lang, Key::new_direct_message)}
                value={(*query).clone()}
                oninput={{
                    let query = query.clone();
                    Callback::from(move |e: InputEvent| {
                        if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                            query.set(el.value());
                        }
                    })
                }}
            />

            if let Some(e) = &*error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }

            <div class="fn-picklist">
                { match (&*load, results.is_empty()) {
                    (Load::Idle, _) => html! {
                        <>
                            if let Some(me) = &me {
                                <div class="fn-picklist__row">
                                    <Ident seed={me.to_string()} size={IdentSize::Xs} />
                                    <div class="fn-grow">
                                        <div>{ t(lang, Key::note_to_self) }</div>
                                        <Addr address={me.clone()} />
                                    </div>
                                    <button
                                        type="button"
                                        class="topcoat-button"
                                        onclick={open_self}
                                    >{ t(lang, Key::open_conversation) }</button>
                                </div>
                            }
                            <Empty art="🔍" title={t(lang, Key::search_for_someone)}
                                   description={t(lang, Key::search_by_username)} />
                        </>
                    },
                    (Load::Loading, _) => html! { <Skeleton rows={3} /> },
                    (Load::Error(e), _) => html! {
                        <Empty art="⚠️" title={t(lang, Key::search_failed)}
                               description={e.clone()} is_error=true />
                    },
                    (_, true) => html! {
                        <Empty
                            art="👤"
                            title={t(lang, Key::no_one_found_for).replace("{query}", query.trim())}
                            description={t(lang, Key::search_by_username)}
                        />
                    },
                    _ => html! {
                        <>
                        { for results.iter().enumerate().map(|(i, u)| {
                            let busy = opening.as_ref() == Some(&u.wallet_address);
                            html! {
                                <div key={u.wallet_address.to_string()} class="fn-picklist__row"
                                     style={format!("--i: {i}")}>
                                    <Ident seed={u.wallet_address.to_string()} size={IdentSize::Xs}
                                           image={u.profile_image.clone()} />
                                    <div class="fn-grow">
                                        <div>{ u.display_name() }</div>
                                        <Addr address={u.wallet_address.clone()} />
                                    </div>
                                    <button
                                        type="button"
                                        class="topcoat-button"
                                        disabled={busy}
                                        onclick={{
                                            let open = open.clone();
                                            let who = u.wallet_address.clone();
                                            Callback::from(move |_: MouseEvent| {
                                                open.emit(who.clone())
                                            })
                                        }}
                                    >{ t(lang, if busy { Key::opening } else { Key::message_verb }) }</button>
                                </div>
                            }
                        }) }
                        </>
                    },
                } }
            </div>
        </Dialog>
    }
}
