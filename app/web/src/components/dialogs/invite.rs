//! Screen 5 — Invite (DESIGN.md §9).

use pocketskynet_core::{RoomId, WalletAddress};
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::actions;
use crate::api::User;
use crate::state::{use_store, Load};

use super::super::common::{Addr, Empty, Ident, IdentSize, Skeleton};
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

/// Screen 5 — Invite (DESIGN.md §9).
#[derive(Properties, PartialEq)]
pub struct InviteProps {
    pub room_id: RoomId,
    pub on_close: Callback<()>,
}

#[function_component(Invite)]
pub fn invite(p: &InviteProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let query = use_state(String::new);
    let results = use_state(Vec::<User>::new);
    let load = use_state(Load::default);
    let invited = use_state(Vec::<WalletAddress>::new);
    let error = use_state(|| Option::<String>::None);

    let room = store.room(&p.room_id).cloned();
    let member_of: Vec<WalletAddress> = room
        .as_ref()
        .map(|r| r.members.iter().map(|m| m.user_address.clone()).collect())
        .unwrap_or_default();
    let encrypted = room.as_ref().is_some_and(|r| r.has_encryption);

    // Debounced search. 250 ms: long enough that typing a full address does not
    // fire ten requests against a 100/min limiter, short enough to feel live.
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

    let do_invite = {
        let store = store.clone();
        let invited = invited.clone();
        let error = error.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |(who, name): (WalletAddress, String)| {
            let store = store.clone();
            let invited = invited.clone();
            let error = error.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.invite(&room_id, &who).await {
                    Ok(()) => {
                        let mut v = (*invited).clone();
                        v.push(who.clone());
                        invited.set(v);
                        toast::success(&store, t(lang, Key::invite_sent).replace("{name}", &name));

                        // Pre-wrap the room key while we are online and holding
                        // it; they may not be online again for days. Best
                        // effort — the invitation already succeeded.
                        if encrypted {
                            if let Err(why) = actions::prewrap_key_for(&store, &room_id, &who).await
                            {
                                toast::warn(
                                    &store,
                                    "They'll get the room key later",
                                    Some(
                                        t(lang, Key::couldnt_send_key_now)
                                            .replace("{reason}", &why.to_string()),
                                    ),
                                );
                            }
                        }
                    }
                    // The server distinguishes "you blocked them" from "they
                    // blocked you"; the UI must not, or it leaks the latter.
                    Err(e) if e.is_forbidden() => {
                        error.set(Some("You can't invite that person.".into()))
                    }
                    Err(e) => error.set(Some(e.user_message())),
                }
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

    html! {
        <Dialog
            title={t(lang, Key::invite_people)}
            description={t(lang, Key::join_once_accept)}
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button--cta" onclick={close_click}>
                    { t(lang, Key::done) }
                </button>
            })}
        >
            <input
                data-autofocus="true"
                class="topcoat-search-input"
                type="search"
                placeholder={t(lang, Key::username_or_address)}
                aria-label={t(lang, Key::search_to_invite)}
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
                        <Empty art="🔍" title={t(lang, Key::search_for_someone)}
                               description={t(lang, Key::search_by_username)} />
                    },
                    (Load::Loading, _) => html! { <Skeleton rows={3} /> },
                    (Load::Error(e), _) => html! {
                        <Empty art="⚠️" title={t(lang, Key::search_failed)} description={e.clone()} is_error=true />
                    },
                    (_, true) => html! {
                        <Empty
                            art="👤"
                            title={t(lang, Key::no_one_found_for).replace("{query}", query.trim())}
                            description="Paste a full wallet address to invite someone who hasn't \
                                         signed in yet."
                        />
                    },
                    _ => html! {
                        <>
                        { for results.iter().enumerate().map(|(i, u)| {
                            let already = member_of.contains(&u.wallet_address);
                            let sent = invited.contains(&u.wallet_address);
                            let name = u.display_name();
                            html! {
                                <div key={u.wallet_address.to_string()} class="fn-picklist__row"
                                     style={format!("--i: {i}")}>
                                    <Ident seed={u.wallet_address.to_string()} size={IdentSize::Xs} image={u.profile_image.clone()} />
                                    <div class="fn-grow">
                                        <div>{ &name }</div>
                                        <Addr address={u.wallet_address.clone()} />
                                    </div>
                                    if already {
                                        <span class="fn-badge fn-badge--muted">{ t(lang, Key::already_a_member) }</span>
                                    } else if sent {
                                        <span class="fn-badge fn-badge--encrypt">{ t(lang, Key::invited) }</span>
                                    } else {
                                        <button
                                            type="button"
                                            class="topcoat-button"
                                            onclick={{
                                                let do_invite = do_invite.clone();
                                                let who = u.wallet_address.clone();
                                                let name = name.clone();
                                                Callback::from(move |_: MouseEvent| {
                                                    do_invite.emit((who.clone(), name.clone()))
                                                })
                                            }}
                                        >{ t(lang, Key::invite) }</button>
                                    }
                                </div>
                            }
                        }) }
                        </>
                    },
                } }
            </div>

            if encrypted {
                <p class="fn-field__help">
                    { "🔒 " }{ t(lang, Key::invite_key_on_accept) }
                </p>
            }
        </Dialog>
    }
}
