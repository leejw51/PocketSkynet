//! The mentions inbox — everything that named you, newest first.
//!
//! A triage surface, not an archive. It answers one question ("what is waiting
//! for me?") and its only real control is "take me there", because every other
//! thing you might do to a message you do in the room it lives in.
//!
//! There is no "mark as read" and no "clear all". A mention is read when its
//! room is read, which opening it already does — a second read state would be
//! a second thing to keep in step with the first, and the two would drift the
//! first time a client crashed between the two calls.

use yew::prelude::*;

use crate::api::mentions::Mention;
use crate::route::Route;
use crate::state::{use_store, Load};

use super::super::common::{Empty, Ident, IdentSize, Skeleton};
use super::super::modal::Modal as Dialog;
use crate::i18n::{t, Key};

/// How many to ask for. The inbox is for triage; anything older is in the room.
const LIMIT: u32 = 50;

#[derive(Properties, PartialEq)]
pub struct MentionsProps {
    pub on_close: Callback<()>,
    pub on_navigate: Callback<Route>,
}

#[function_component(Mentions)]
pub fn mentions(p: &MentionsProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let items = use_state(Vec::<Mention>::new);
    let load = use_state(Load::default);

    {
        let store = store.clone();
        let items = items.clone();
        let load = load.clone();
        use_effect_with((), move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.mentions(LIMIT).await {
                    Ok(v) => {
                        items.set(v);
                        load.set(Load::Ready);
                    }
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
            || ()
        });
    }

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
            title={t(lang, Key::mentions)}
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button" onclick={close_click}>
                    { t(lang, Key::done) }
                </button>
            })}
        >
            { match (&*load, items.is_empty()) {
                (Load::Loading, true) => html! { <Skeleton rows={4} /> },
                (Load::Error(e), _) => html! {
                    <Empty art="⚠️" title={t(lang, Key::search_failed)}
                           description={e.clone()} is_error=true />
                },
                (_, true) => html! {
                    <Empty art="@" title={t(lang, Key::mentions_empty)}
                           description={t(lang, Key::mentions_empty_hint)} />
                },
                _ => html! {
                    <div class="fn-picklist">
                    { for items.iter().map(|m| {
                        let sender = m.message.sender.as_ref();
                        let name = sender
                            .map(|u| u.display_name())
                            .unwrap_or_else(|| m.message.sender_address.abbreviated());
                        // A DM has no name of its own, so the server's
                        // placeholder must not reach the screen here either.
                        // The room list is where the derived title lives.
                        let room_label = store
                            .room(&m.room_id)
                            .zip(me.as_ref())
                            .map(|(r, me)| r.title_for(me))
                            .unwrap_or_else(|| m.room_name.clone());
                        let go = {
                            let on_navigate = p.on_navigate.clone();
                            let on_close = p.on_close.clone();
                            let room_id = m.room_id.clone();
                            Callback::from(move |_: MouseEvent| {
                                on_close.emit(());
                                on_navigate.emit(Route::Room(room_id.clone()));
                            })
                        };
                        html! {
                            <button
                                key={m.message.id.to_string()}
                                type="button"
                                class={classes!(
                                    "fn-picklist__row",
                                    "fn-mention-row",
                                    m.is_unread.then_some("is-unread"),
                                )}
                                onclick={go}
                            >
                                <Ident
                                    seed={m.message.sender_address.to_string()}
                                    size={IdentSize::Xs}
                                    image={sender.and_then(|u| u.profile_image.clone())}
                                />
                                <div class="fn-grow fn-mention-row__body">
                                    <div class="fn-mention-row__head">
                                        <strong>{ name }</strong>
                                        <span class="fn-mention-row__room">{ room_label }</span>
                                        <time>{ crate::format::hhmm(
                                            m.message.message_timestamp,
                                            crate::format::tz_offset_minutes(),
                                        ) }</time>
                                    </div>
                                    // Encrypted content is unreadable here on
                                    // purpose: this dialog holds no room keys,
                                    // and decrypting a dozen rooms' worth to
                                    // fill a preview would be the one place the
                                    // app quietly widened what it decrypts.
                                    <p class="fn-mention-row__text">{
                                        if m.message.is_encrypted {
                                            t(lang, Key::encrypted_message).to_owned()
                                        } else {
                                            m.message.content.clone()
                                        }
                                    }</p>
                                </div>
                            </button>
                        }
                    }) }
                    </div>
                },
            } }
        </Dialog>
    }
}
