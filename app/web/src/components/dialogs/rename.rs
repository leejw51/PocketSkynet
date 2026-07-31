//! Rename a room. Admin-only, server-enforced.

use pocketskynet_core::RoomId;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::actions;
use crate::state::use_store;

use super::super::common::BusyButton;
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

/// Rename a room. Admin-only, server-enforced.
#[derive(Properties, PartialEq)]
pub struct RenameProps {
    pub room_id: RoomId,
    pub current: String,
    pub on_close: Callback<()>,
}

#[function_component(RenameRoom)]
pub fn rename_room(p: &RenameProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let name = use_state(|| p.current.clone());
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);

    let submit = {
        let store = store.clone();
        let name = name.clone();
        let busy = busy.clone();
        let error = error.clone();
        let room_id = p.room_id.clone();
        let on_close = p.on_close.clone();
        Callback::from(move |_: MouseEvent| {
            let new_name = name.trim().to_owned();
            if new_name.is_empty() {
                error.set(Some("Enter a room name.".into()));
                return;
            }
            busy.set(true);
            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let room_id = room_id.clone();
            let on_close = on_close.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.rename_room(&room_id, &new_name).await {
                    Ok(_) => {
                        // Neutral, not emerald: emerald means "encryption held",
                        // never generic success.
                        toast::neutral(&store, "Room renamed");
                        actions::refresh_rooms(store.clone()).await;
                        on_close.emit(());
                    }
                    Err(e) => error.set(Some(e.user_message())),
                }
                busy.set(false);
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
            title={t(lang, Key::rename_room)}
            busy={*busy}
            on_close={close}
            footer={Some(html! {
                <>
                    <button type="button" class="topcoat-button" disabled={*busy} onclick={close_click}>
                        { t(lang, Key::cancel) }
                    </button>
                    <BusyButton label={t(lang, Key::save)} busy={*busy} onclick={submit} />
                </>
            })}
        >
            <div class="fn-field">
                <label class="fn-field__label" for="rename-input">{ t(lang, Key::room_name) }</label>
                <input
                    id="rename-input"
                    data-autofocus="true"
                    class="topcoat-text-input"
                    type="text"
                    maxlength="64"
                    value={(*name).clone()}
                    oninput={{
                        let name = name.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                                name.set(el.value());
                            }
                        })
                    }}
                />
                if let Some(e) = &*error {
                    <p class="fn-field__error" role="alert">{ e }</p>
                }
            </div>
        </Dialog>
    }
}
