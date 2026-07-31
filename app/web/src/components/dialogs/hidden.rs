//! Hidden rooms (DESIGN.md §13).

use pocketskynet_core::RoomId;
use yew::prelude::*;

use crate::actions;
use crate::api::HiddenRoom;
use crate::format;
use crate::state::{use_store, Load};

use super::super::common::{BusyButton, Empty, Ident, IdentSize, Skeleton};
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

/// Hidden rooms (DESIGN.md §13).
#[derive(Properties, PartialEq)]
pub struct HiddenProps {
    pub on_close: Callback<()>,
}

#[function_component(HiddenRooms)]
pub fn hidden_rooms(p: &HiddenProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let rooms = use_state(Vec::<HiddenRoom>::new);
    let load = use_state(Load::default);
    let busy = use_state(|| Option::<RoomId>::None);

    {
        let store = store.clone();
        let rooms = rooms.clone();
        let load = load.clone();
        use_effect_with((), move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.hidden_rooms().await {
                    Ok(v) => {
                        rooms.set(v);
                        load.set(Load::Ready);
                    }
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
            || ()
        });
    }

    let unhide = {
        let store = store.clone();
        let rooms = rooms.clone();
        let busy = busy.clone();
        Callback::from(move |id: RoomId| {
            busy.set(Some(id.clone()));
            let store = store.clone();
            let rooms = rooms.clone();
            let busy = busy.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if store.client.unhide_room(&id).await.is_ok() {
                    rooms.set(rooms.iter().filter(|h| h.room_id != id).cloned().collect());
                    actions::refresh_rooms(store.clone()).await;
                    toast::success(&store, t(lang, Key::room_unhidden));
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
    let tz = format::tz_offset_minutes();

    html! {
        <Dialog
            title={t(lang, Key::hidden_rooms)}
            description={t(lang, Key::hiding_a_room_note)}
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button--cta" onclick={close_click}>{ t(lang, Key::done) }</button>
            })}
        >
            { match (&*load, rooms.is_empty()) {
                (Load::Loading, _) => html! { <Skeleton rows={2} /> },
                (Load::Error(e), _) => html! {
                    <Empty art="⚠️" title={t(lang, Key::couldnt_load_hidden)}
                           description={e.clone()} is_error=true />
                },
                (_, true) => html! {
                    <Empty art="👁" title={t(lang, Key::no_hidden_rooms)}
                           description={t(lang, Key::hiding_a_room_note)} />
                },
                _ => html! {
                    <div class="fn-picklist">
                        { for rooms.iter().enumerate().map(|(i, h)| {
                            let acting = busy.as_ref() == Some(&h.room_id);
                            html! {
                                <div key={h.room_id.to_string()} class="fn-picklist__row"
                                     style={format!("--i: {i}")}>
                                    <Ident seed={h.room_id.to_string()} size={IdentSize::Xs} />
                                    <div class="fn-grow">
                                        <div>{ h.room.as_ref().map(|r| r.room.name.clone())
                                                .unwrap_or_else(|| h.room_id.to_string()) }</div>
                                        if let Some(at) = h.created_at.as_deref().and_then(format::parse_iso8601_ms) {
                                            <div class="fn-muted">
                                                { format!("hidden {}", format::short_date(at, tz)) }
                                            </div>
                                        }
                                    </div>
                                    <BusyButton
                                        label={t(lang, Key::unhide)}
                                        class="topcoat-button"
                                        busy={acting}
                                        disabled={busy.is_some()}
                                        onclick={{
                                            let unhide = unhide.clone();
                                            let id = h.room_id.clone();
                                            Callback::from(move |_: MouseEvent| unhide.emit(id.clone()))
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
