//! Screen 8 — Blocked people (DESIGN.md §12).

use pocketskynet_core::WalletAddress;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::actions;
use crate::format;
use crate::state::use_store;

use super::super::common::{Addr, BusyButton, Empty, Ident, IdentSize};
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

/// Screen 8 — Blocked people (DESIGN.md §12).
#[derive(Properties, PartialEq)]
pub struct BlockedProps {
    pub on_close: Callback<()>,
}

#[function_component(Blocked)]
pub fn blocked(p: &BlockedProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let adding = use_state(|| false);
    let draft = use_state(String::new);
    let busy = use_state(|| Option::<WalletAddress>::None);
    let error = use_state(|| Option::<String>::None);

    let me = store.me().cloned();
    let tz = format::tz_offset_minutes();

    // Live validation: `^0x[a-fA-F0-9]{40}$`, plus "not your own address".
    let parsed = WalletAddress::new(draft.trim()).ok();
    let is_self = parsed.as_ref().is_some_and(|a| Some(a) == me.as_ref());
    let already = parsed
        .as_ref()
        .is_some_and(|a| store.blocked.iter().any(|b| &b.blocked_address == a));

    let validation = if draft.trim().is_empty() {
        None
    } else if parsed.is_none() {
        Some("That's not a wallet address. It should be 0x followed by 40 hex characters.")
    } else if is_self {
        Some("You can't block yourself.")
    } else if already {
        Some("You already blocked this address.")
    } else {
        None
    };

    let act = {
        let store = store.clone();
        let busy = busy.clone();
        let error = error.clone();
        let draft = draft.clone();
        let adding = adding.clone();
        Callback::from(move |(who, block): (WalletAddress, bool)| {
            if busy.is_some() {
                return;
            }
            busy.set(Some(who.clone()));
            error.set(None);
            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let draft = draft.clone();
            let adding = adding.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = if block {
                    store.client.block_user(&who).await
                } else {
                    store.client.unblock_user(&who).await
                };
                match result {
                    Ok(()) => {
                        actions::refresh_blocks(store.clone()).await;
                        let short = who.abbreviated();
                        if block {
                            toast::success(
                                &store,
                                t(lang, Key::blocked_someone).replace("{short}", &short),
                            );
                            draft.set(String::new());
                            adding.set(false);
                        } else {
                            toast::success(
                                &store,
                                t(lang, Key::unblocked_someone).replace("{short}", &short),
                            );
                        }
                    }
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

    html! {
        <Dialog
            title={t(lang, Key::blocked_people)}
            description={t(lang, Key::blocked_note)}
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button--cta" onclick={close_click}>{ t(lang, Key::done) }</button>
            })}
        >
            if *adding {
                <div class="fn-field">
                    <label class="fn-field__label" for="block-addr">{ t(lang, Key::wallet_address_label) }</label>
                    <input
                        id="block-addr"
                        data-autofocus="true"
                        class="topcoat-text-input"
                        style="font-family: var(--fn-font-mono)"
                        type="text"
                        placeholder="0x…"
                        aria-invalid={validation.is_some().then_some("true")}
                        value={(*draft).clone()}
                        oninput={{
                            let draft = draft.clone();
                            Callback::from(move |e: InputEvent| {
                                if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                                    draft.set(el.value());
                                }
                            })
                        }}
                    />
                    if let Some(v) = validation {
                        <p class="fn-field__error" role="alert">{ v }</p>
                    }
                    <div class="fn-row">
                        <button
                            type="button"
                            class="topcoat-button"
                            onclick={{
                                let adding = adding.clone();
                                let draft = draft.clone();
                                Callback::from(move |_: MouseEvent| {
                                    adding.set(false);
                                    draft.set(String::new());
                                })
                            }}
                        >{ t(lang, Key::cancel) }</button>
                        <BusyButton
                            label={t(lang, Key::block)}
                            class="topcoat-button--danger"
                            busy={busy.is_some()}
                            disabled={parsed.is_none() || validation.is_some()}
                            onclick={{
                                let act = act.clone();
                                let parsed = parsed.clone();
                                Callback::from(move |_: MouseEvent| {
                                    if let Some(a) = parsed.clone() {
                                        act.emit((a, true));
                                    }
                                })
                            }}
                        />
                    </div>
                </div>
            } else {
                <button
                    type="button"
                    class="topcoat-button"
                    style="width:100%"
                    onclick={{
                        let adding = adding.clone();
                        Callback::from(move |_: MouseEvent| adding.set(true))
                    }}
                >{ "+ " }{ t(lang, Key::block_someone) }</button>
            }

            if let Some(e) = &*error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }

            if store.blocked.is_empty() {
                <Empty
                    art="🚫"
                    title={t(lang, Key::no_one_blocked)}
                    description={t(lang, Key::block_from_row)}
                />
            } else {
                <div class="fn-picklist">
                    { for store.blocked.iter().enumerate().map(|(i, b)| {
                        let acting = busy.as_ref() == Some(&b.blocked_address);
                        html! {
                            <div key={b.blocked_address.to_string()} class="fn-picklist__row"
                                 style={format!("--i: {i}")}>
                                <Ident seed={b.blocked_address.to_string()} size={IdentSize::Xs} />
                                <div class="fn-grow">
                                    <Addr address={b.blocked_address.clone()} />
                                    if let Some(at) = b.created_at.as_deref().and_then(format::parse_iso8601_ms) {
                                        <div class="fn-muted">
                                            { format!("blocked {}", format::short_date(at, tz)) }
                                        </div>
                                    }
                                </div>
                                <BusyButton
                                    label={t(lang, Key::unblock)}
                                    class="topcoat-button"
                                    busy={acting}
                                    disabled={busy.is_some()}
                                    onclick={{
                                        let act = act.clone();
                                        let who = b.blocked_address.clone();
                                        Callback::from(move |_: MouseEvent| act.emit((who.clone(), false)))
                                    }}
                                />
                            </div>
                        }
                    }) }
                </div>
                <p class="fn-muted">
                    { t(lang, if store.blocked.len() == 1 { Key::blocked_count_one } else { Key::blocked_count_many })
                        .replace("{n}", &store.blocked.len().to_string()) }
                </p>
            }
        </Dialog>
    }
}
