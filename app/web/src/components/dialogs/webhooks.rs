//! Incoming webhooks for one room (API.md §17) — create, copy the URL, revoke.
//!
//! Admin-only by construction: the menu item that opens this is gated the
//! same way "Manage admins" is, and the server refuses everyone else anyway.
//! The URL each row copies is the credential itself, which shapes the copy in
//! this dialog — it says so, plainly, instead of pretending the link is inert.

use pocketskynet_core::RoomId;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::api::Webhook;
use crate::state::{use_store, Load};

use super::super::common::{BusyButton, Empty, Ident, IdentSize, Skeleton};
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

#[derive(Properties, PartialEq)]
pub struct WebhooksProps {
    pub room_id: RoomId,
    pub on_close: Callback<()>,
}

/// The absolute URL an external system posts to.
///
/// The server sends a *path*: it cannot know which host, port or tunnel the
/// operator exposed, but the page this dialog is rendered on was reached
/// through exactly that surface, so its origin is the one address known to
/// work from outside.
fn absolute_url(path: &str) -> String {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    format!("{origin}{path}")
}

#[function_component(ManageWebhooks)]
pub fn manage_webhooks(p: &WebhooksProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let hooks = use_state(Vec::<Webhook>::new);
    let load = use_state(Load::default);
    let draft = use_state(String::new);
    let creating = use_state(|| false);
    let busy = use_state(|| Option::<String>::None);
    let error = use_state(|| Option::<String>::None);

    {
        let store = store.clone();
        let hooks = hooks.clone();
        let load = load.clone();
        let room_id = p.room_id.clone();
        use_effect_with(room_id.clone(), move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.webhooks(&room_id).await {
                    Ok(v) => {
                        hooks.set(v);
                        load.set(Load::Ready);
                    }
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
            || ()
        });
    }

    let create = {
        let store = store.clone();
        let hooks = hooks.clone();
        let draft = draft.clone();
        let creating = creating.clone();
        let error = error.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |_: MouseEvent| {
            let name = draft.trim().to_owned();
            if name.is_empty() || *creating {
                return;
            }
            creating.set(true);
            error.set(None);
            let store = store.clone();
            let hooks = hooks.clone();
            let draft = draft.clone();
            let creating = creating.clone();
            let error = error.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.create_webhook(&room_id, &name).await {
                    Ok(hook) => {
                        // Newest first, matching the list endpoint's order.
                        let mut next = vec![hook];
                        next.extend(hooks.iter().cloned());
                        hooks.set(next);
                        draft.set(String::new());
                    }
                    Err(e) => error.set(Some(e.user_message())),
                }
                creating.set(false);
            });
        })
    };

    let copy = {
        let store = store.clone();
        Callback::from(move |url: String| {
            let store = store.clone();
            super::super::common::copy_then(&absolute_url(&url), move |ok| {
                if ok {
                    toast::success(&store, t(lang, Key::webhook_url_copied));
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

    let revoke = {
        let store = store.clone();
        let hooks = hooks.clone();
        let busy = busy.clone();
        let error = error.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |id: String| {
            if busy.is_some() {
                return;
            }
            busy.set(Some(id.clone()));
            error.set(None);
            let store = store.clone();
            let hooks = hooks.clone();
            let busy = busy.clone();
            let error = error.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.revoke_webhook(&room_id, &id).await {
                    Ok(()) => hooks.set(hooks.iter().filter(|h| h.id != id).cloned().collect()),
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
            title={t(lang, Key::webhooks_title)}
            description={t(lang, Key::webhooks_note)}
            wide=true
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button--cta" onclick={close_click}>{ t(lang, Key::done) }</button>
            })}
        >
            <div class="fn-row">
                <input
                    class="topcoat-text-input fn-grow"
                    type="text"
                    placeholder={t(lang, Key::webhook_name_placeholder)}
                    value={(*draft).clone()}
                    oninput={{
                        let draft = draft.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: HtmlInputElement = e.target_unchecked_into();
                            draft.set(input.value());
                        })
                    }}
                />
                <BusyButton
                    label={t(lang, Key::create)}
                    class="topcoat-button--cta"
                    busy={*creating}
                    disabled={draft.trim().is_empty()}
                    onclick={create}
                />
            </div>
            <p class="fn-field__help">{ t(lang, Key::webhook_how) }</p>

            if let Some(e) = &*error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }

            { match (&*load, hooks.is_empty()) {
                (Load::Loading, _) => html! { <Skeleton rows={2} /> },
                (Load::Error(e), _) => html! {
                    <Empty art="⚠️" title={t(lang, Key::webhooks_title)}
                           description={e.clone()} is_error=true />
                },
                (_, true) => html! {
                    <Empty art="🤖" title={t(lang, Key::webhooks_title)}
                           description={t(lang, Key::webhooks_empty)} />
                },
                _ => html! {
                    <div class="fn-picklist">
                        { for hooks.iter().enumerate().map(|(i, h)| {
                            let acting = busy.as_deref() == Some(h.id.as_str());
                            html! {
                                <div key={h.id.clone()} class="fn-picklist__row"
                                     style={format!("--i: {i}")}>
                                    // The same portrait its posts wear in the
                                    // room, so "which feed is this" is a
                                    // glance rather than a comparison.
                                    <Ident seed={h.sender_address.to_string()} size={IdentSize::Xs} />
                                    <div class="fn-grow">
                                        <div>{ &h.name }</div>
                                        // Truncated by CSS, complete in the
                                        // clipboard: the token is long by
                                        // design and nobody transcribes it.
                                        <div class="fn-muted fn-truncate">{ absolute_url(&h.url) }</div>
                                    </div>
                                    <button
                                        type="button"
                                        class="topcoat-button"
                                        onclick={{
                                            let copy = copy.clone();
                                            let url = h.url.clone();
                                            Callback::from(move |_: MouseEvent| copy.emit(url.clone()))
                                        }}
                                    >{ super::super::icons::copy(16) }{ " " }{ t(lang, Key::copy) }</button>
                                    <BusyButton
                                        label={t(lang, Key::revoke)}
                                        class="topcoat-button--danger"
                                        busy={acting}
                                        disabled={busy.is_some()}
                                        onclick={{
                                            let revoke = revoke.clone();
                                            let id = h.id.clone();
                                            Callback::from(move |_: MouseEvent| revoke.emit(id.clone()))
                                        }}
                                    />
                                </div>
                            }
                        }) }
                    </div>
                },
            } }
        </Dialog>
    }
}
