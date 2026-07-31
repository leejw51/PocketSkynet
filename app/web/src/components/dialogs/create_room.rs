//! Screen 4 — Create room (DESIGN.md §8), and the one-click version of it.
//!
//! Both buttons here — and the room list's ⚡ shortcut — drive the same
//! [`actions::create_room_flow`]; this dialog owns only the *form*.

use pocketskynet_core::RoomId;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::actions;
use crate::state::use_store;

use super::super::common::BusyButton;
use super::super::icons;
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

/// Screen 4 — Create room (DESIGN.md §8).
#[derive(Properties, PartialEq)]
pub struct CreateRoomProps {
    pub on_close: Callback<()>,
    pub on_created: Callback<RoomId>,
}

#[function_component(CreateRoom)]
pub fn create_room(p: &CreateRoomProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let name = use_state(String::new);
    let description = use_state(String::new);
    // Encryption defaults **on**, and cannot be turned on later — the dialog
    // says so, because a room created plaintext is plaintext forever.
    let encrypt = use_state(|| true);
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    // Set when the room was created but its key could not be established.
    let downgraded = use_state(|| Option::<String>::None);

    // The whole flow — create, key, refresh, open — with the three inputs
    // passed in rather than read from state. Both buttons drive it, and "fast"
    // must not depend on a `use_state` write having been flushed first: the
    // fields it fills in are not readable in the same tick it fills them.
    let run = {
        let store = store.clone();
        let busy = busy.clone();
        let error = error.clone();
        let downgraded = downgraded.clone();
        let on_created = p.on_created.clone();
        Callback::from(
            move |(room_name, description, want_encryption): (String, String, bool)| {
                if *busy {
                    return;
                }
                if room_name.is_empty() {
                    error.set(Some("Enter a room name.".into()));
                    return;
                }
                busy.set(true);
                error.set(None);

                let store = store.clone();
                let busy = busy.clone();
                let error = error.clone();
                let downgraded = downgraded.clone();
                let on_created = on_created.clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match actions::create_room_flow(
                        &store,
                        &room_name,
                        &description,
                        want_encryption,
                    )
                    .await
                    {
                        // The user MUST see this. Silently downgrading an E2EE
                        // room is the worst possible outcome (DESIGN.md §8);
                        // the dialog stays open, turned into the warning.
                        Ok((_, Some(why))) => downgraded.set(Some(why)),
                        Ok((room_id, None)) => {
                            toast::success(&store, t(lang, Key::room_created));
                            // Creating a room joins it — the server makes the
                            // creator its first member and admin — so opening
                            // it is the last step of the same action.
                            on_created.emit(room_id);
                        }
                        Err(e) => error.set(Some(e)),
                    }
                    busy.set(false);
                });
            },
        )
    };

    let submit = {
        let run = run.clone();
        let name = name.clone();
        let description = description.clone();
        let encrypt = encrypt.clone();
        Callback::from(move |_: MouseEvent| {
            run.emit((
                name.trim().to_owned(),
                description.trim().to_owned(),
                *encrypt,
            ));
        })
    };

    // One click: name it, describe it, encrypt it, greet it, open it. Runs
    // `actions::fast_create_room` — the same flow as the room list's ⚡ button
    // — rather than this form's plain submit, because the fast path also posts
    // the hello-world greeting, and two "fast" buttons that produce different
    // rooms would be a bug wearing the same label.
    let fast = {
        let store = store.clone();
        let name = name.clone();
        let description = description.clone();
        let encrypt = encrypt.clone();
        let error = error.clone();
        let busy = busy.clone();
        let downgraded = downgraded.clone();
        let on_created = p.on_created.clone();
        let can_encrypt = store.auth.can_decrypt();
        Callback::from(move |_: MouseEvent| {
            if *busy {
                return;
            }
            // Fast implies encrypted, and encryption needs unlocked keys. Doing
            // it anyway would produce a plaintext room from a button whose whole
            // promise is an encrypted one.
            if !can_encrypt {
                error.set(Some(t(lang, Key::fast_room_needs_phrase).to_owned()));
                return;
            }
            let (room_name, room_description) = actions::auto_room(store.language);
            // Shown, not just sent: the user should be able to read what was
            // created without going looking for it, and rename it from here if
            // they do not like it.
            name.set(room_name.clone());
            description.set(room_description.clone());
            encrypt.set(true);
            busy.set(true);
            error.set(None);

            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let downgraded = downgraded.clone();
            let on_created = on_created.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match actions::fast_create_room(&store, &room_name, &room_description).await {
                    Ok((_, Some(why))) => downgraded.set(Some(why)),
                    Ok((room_id, None)) => {
                        toast::success(&store, t(lang, Key::room_created));
                        on_created.emit(room_id);
                    }
                    Err(e) => error.set(Some(e)),
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

    let footer = if downgraded.is_some() {
        html! {
            <button type="button" class="topcoat-button--cta" onclick={close_click.clone()}>
                { t(lang, Key::close) }
            </button>
        }
    } else {
        html! {
            <>
                <button type="button" class="topcoat-button" disabled={*busy} onclick={close_click.clone()}>
                    { t(lang, Key::cancel) }
                </button>
                <BusyButton label={t(lang, Key::create)} busy={*busy} onclick={submit} />
            </>
        }
    };

    let count = name.chars().count();

    html! {
        <Dialog
            title={t(lang, Key::create_a_room)}
            description={t(lang, Key::rooms_are_private)}
            busy={*busy}
            on_close={close}
            footer={Some(footer)}
        >
            if let Some(why) = &*downgraded {
                <div class="fn-banner fn-banner--warn" role="alert">
                    { t(lang, Key::room_created_unencrypted).replace("{reason}", why) }
                </div>
            }

            if downgraded.is_none() {
                <div class="fn-login__hero">
                    <button
                        type="button"
                        class="fn-hero-btn topcoat-button--large--cta"
                        disabled={*busy}
                        onclick={fast}
                    >
                        { icons::bolt(20) }
                        <span class="fn-hero-btn__label">
                            <b>{ t(lang, Key::fast_create_room) }</b>
                            <small>{ t(lang, Key::fast_room_tagline) }</small>
                        </span>
                    </button>
                </div>
                <div class="fn-rule">{ t(lang, Key::or_set_it_up_yourself) }</div>
            }

            <div class="fn-field">
                <label class="fn-field__label" for="room-name">{ t(lang, Key::room_name) }</label>
                <input
                    id="room-name"
                    data-autofocus="true"
                    class="topcoat-text-input"
                    type="text"
                    maxlength="64"
                    readonly={*busy}
                    aria-invalid={error.is_some().then_some("true")}
                    value={(*name).clone()}
                    oninput={{
                        let name = name.clone();
                        let error = error.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                                name.set(el.value());
                                error.set(None);
                            }
                        })
                    }}
                />
                if count >= 48 {
                    <p class="fn-field__help fn-nums">{ format!("{count} / 64") }</p>
                }
                if let Some(e) = &*error {
                    <p class="fn-field__error" role="alert">{ e }</p>
                }
            </div>

            <div class="fn-field">
                <label class="fn-field__label" for="room-desc">{ t(lang, Key::description_optional) }</label>
                <textarea
                    id="room-desc"
                    class="topcoat-textarea"
                    rows="2"
                    maxlength="500"
                    readonly={*busy}
                    value={(*description).clone()}
                    oninput={{
                        let description = description.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlTextAreaElement>() {
                                description.set(el.value());
                            }
                        })
                    }}
                />
            </div>

            <div class="fn-toggle-row" data-on={encrypt.to_string()}>
                <label class="fn-row" for="room-encrypt">
                    <input
                        id="room-encrypt"
                        type="checkbox"
                        class="topcoat-checkbox__input"
                        checked={*encrypt}
                        disabled={*busy || !store.auth.can_decrypt()}
                        onchange={{
                            let encrypt = encrypt.clone();
                            Callback::from(move |_: Event| encrypt.set(!*encrypt))
                        }}
                    />
                    <span class="fn-grow">
                        <strong>{ "🔒 " }{ t(lang, Key::encrypt_this_room) }</strong>
                        <p class="fn-field__help">{ t(lang, Key::encrypt_this_room_hint) }</p>
                        if !store.auth.can_decrypt() {
                            <p class="fn-field__error">
                                { t(lang, Key::unlock_to_encrypt) }
                            </p>
                        }
                    </span>
                </label>
            </div>
        </Dialog>
    }
}
