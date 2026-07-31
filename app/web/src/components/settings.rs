//! Screen 9 — Settings & profile (DESIGN.md §13), plus Screen 10 — Not found.
//!
//! Order is deliberate and never changes: identity, then preferences, then the
//! two irreversible rows, visually separated. There is nothing editable in the
//! profile card — the wallet *is* the profile.

use yew::prelude::*;

use crate::i18n::{t, Key, Lang};
use crate::route::Route;
use crate::session::{ConnectionMode, FontFace, FontScale, ShellLayout, Theme};
use crate::state::{use_store, Action, Confirm, ConfirmAction, Modal};

use super::common::{Addr, Back, Ident, IdentSize};
use super::icons;
use super::toast;

#[derive(Properties, PartialEq)]
pub struct SettingsProps {
    pub on_navigate: Callback<Route>,
}

#[function_component(Settings)]
pub fn settings(p: &SettingsProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let offline = !store.online;

    let Some(address) = store.auth.address().cloned() else {
        return html! {};
    };
    let username = store.auth.username().unwrap_or("").to_owned();

    let copy_address = {
        let store = store.clone();
        let address = address.clone();
        Callback::from(move |_: MouseEvent| {
            if super::common::copy_to_clipboard(&address.to_checksummed()) {
                toast::success(&store, t(lang, Key::address_copied));
            } else {
                toast::error(
                    &store,
                    t(lang, Key::couldnt_copy),
                    Some(t(lang, Key::clipboard_blocked).into()),
                );
            }
        })
    };

    // Read once per render rather than held in state: the only things that
    // change it are on this screen, and both re-render it.
    let stored = crate::vault::StoredWallet::load();

    let copy_phrase = |phrase: String, store: crate::state::Store| {
        Callback::from(move |_: MouseEvent| {
            if super::common::copy_to_clipboard(&phrase) {
                toast::success(&store, t(lang, Key::phrase_copied));
            } else {
                toast::error(
                    &store,
                    t(lang, Key::couldnt_copy),
                    Some(t(lang, Key::clipboard_blocked).into()),
                );
            }
        })
    };

    let set_theme = |t: Theme, store: crate::state::Store| {
        Callback::from(move |_: MouseEvent| store.dispatch(Action::SetTheme(t)))
    };
    let set_mode = |m: ConnectionMode, store: crate::state::Store| {
        Callback::from(move |_: MouseEvent| store.dispatch(Action::SetMode(m)))
    };
    let open = |modal: Modal, store: crate::state::Store| {
        Callback::from(move |_: MouseEvent| store.dispatch(Action::OpenModal(modal.clone())))
    };
    let confirm = |c: Confirm, store: crate::state::Store| {
        Callback::from(move |_: MouseEvent| {
            store.dispatch(Action::OpenModal(Modal::Confirm(c.clone())))
        })
    };

    html! {
        <>
            <div class="topcoat-navigation-bar">
                <Back onclick={{
                    let on_navigate = p.on_navigate.clone();
                    Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Rooms))
                }} />
                <h1 class="topcoat-navigation-bar__title">{ t(lang, Key::nav_settings) }</h1>
            </div>

            <div class="fn-scroll fn-stack" style="padding: var(--fn-s4)">
                <section class="fn-picklist__row" aria-label={t(lang, Key::profile)}>
                    <Ident
                        seed={address.to_string()}
                        size={IdentSize::Xl}
                        is_self=true
                        image={store.auth.profile_image().map(str::to_owned)}
                        zoom={crate::components::common::Zoom {
                            title: username.clone(),
                            subtitle: Some(address.to_checksummed()),
                            copy: Some(address.to_checksummed()),
                        }}
                    />
                    <div class="fn-grow fn-stack">
                        <strong>{ &username }</strong>
                        <div class="fn-row">
                            <Addr address={address.clone()} full=true />
                            <button
                                type="button"
                                class="topcoat-icon-button--quiet"
                                aria-label={t(lang, Key::copy_wallet_address).replace("{address}", &address.to_checksummed())}
                                onclick={copy_address}
                            >{ icons::copy(16) }</button>
                        </div>
                        // The network *this wallet* is on — which is what a line
                        // sitting under a wallet address is read as. It used to
                        // print `store.chain`, the chain the **server** anchors
                        // message hashes to; the two were the same value while
                        // both defaulted to testnet, and the moment the wallet
                        // could be switched they disagreed, leaving this card
                        // saying TESTNET under a MAINNET ribbon.
                        <p class="fn-muted">
                            { match store.active_network() {
                                Some(n) => match n.chain_id {
                                    Some(id) => format!("{} · chain {}", n.name.to_uppercase(), id),
                                    None => n.name.to_uppercase(),
                                },
                                // Before `GET /api/networks` answers, fall back
                                // to the server's chain rather than an empty
                                // line — it is the same value on a default
                                // install and never blank.
                                None if !store.chain.chain_name.is_empty() => {
                                    format!("{} · chain {}", store.chain.chain_name, store.chain.chain_id)
                                }
                                None => "Chain not configured".to_owned(),
                            } }
                        </p>
                        if !store.auth.can_decrypt() {
                            // The locked state, stated plainly rather than left
                            // for the user to infer from sealed bubbles.
                            <p class="fn-field__error">{ t(lang, Key::encryption_locked) }</p>
                        }
                    </div>
                </section>

                <section class="fn-stack" aria-label={t(lang, Key::profile_image)}>
                    <h2 class="topcoat-list__header">{ t(lang, Key::profile_image) }</h2>
                    <super::avatar::AvatarPicker />
                </section>

                <h2 class="topcoat-list__header">{ t(lang, Key::preferences) }</h2>
                <div class="topcoat-list__container">
                    // Deliberately no network control here. The chain is the
                    // deployment's, set by `VITE_CHAIN_ID` on the server and
                    // reported through `GET /api/networks` — the identity card
                    // above states which one, and the ribbon says so on every
                    // screen. A per-browser switch would let two people on one
                    // deployment spend on different chains, and let either pick
                    // a chain the server does not anchor to.
                    // Wide viewports only (the class hides it below the
                    // two-pane breakpoint): a phone has one column whichever
                    // way this points, and a control that visibly does
                    // nothing teaches people to stop believing the controls.
                    // Language sits first: it is the one preference that
                    // changes every other label on this screen, so a reader
                    // who cannot read the rest needs to find it without
                    // parsing anything above it. The options are endonyms —
                    // "한국어", not "Korean" — because the person looking for
                    // Korean is exactly the person who cannot read "Korean".
                    <div class="fn-picklist__row">
                        { icons::globe(18) }
                        <span class="fn-grow">{ t(lang, Key::language) }</span>
                        <div class="fn-row fn-row--wrap" role="radiogroup" aria-label={t(lang, Key::language)}>
                            { for Lang::ALL.into_iter().map(|l| {
                                let store = store.clone();
                                html! {
                                    <button
                                        type="button"
                                        role="radio"
                                        class="topcoat-button"
                                        lang={l.tag()}
                                        aria-checked={(lang == l).to_string()}
                                        onclick={Callback::from(move |_: MouseEvent| {
                                            store.dispatch(Action::SetLanguage(l));
                                        })}
                                    >{ l.endonym() }</button>
                                }
                            }) }
                        </div>
                    </div>

                    <div class="fn-picklist__row fn-picklist__row--wide-only">
                        { icons::columns(18) }
                        <span class="fn-grow">{ t(lang, Key::layout) }</span>
                        <div class="fn-row" role="radiogroup" aria-label={t(lang, Key::pane_layout)}>
                            { for [
                                (ShellLayout::Horizontal, icons::columns(16), t(lang, Key::layout_side_by_side)),
                                (ShellLayout::Vertical, icons::rows(16), t(lang, Key::layout_stacked)),
                            ].into_iter().map(|(l, glyph, label)| {
                                let store = store.clone();
                                html! {
                                    <button
                                        type="button"
                                        role="radio"
                                        class="topcoat-button"
                                        aria-checked={(store.shell_layout == l).to_string()}
                                        onclick={Callback::from(move |_: MouseEvent| {
                                            store.dispatch(Action::SetShellLayout(l));
                                        })}
                                    >{ glyph }{ label }</button>
                                }
                            }) }
                        </div>
                    </div>

                    <div class="fn-picklist__row">
                        { icons::moon_sun(18) }
                        <span class="fn-grow">{ t(lang, Key::appearance) }</span>
                        <div class="fn-row" role="radiogroup" aria-label={t(lang, Key::appearance)}>
                            { for [(Theme::Light, t(lang, Key::theme_light)), (Theme::Dark, t(lang, Key::theme_dark)),
                                   (Theme::System, t(lang, Key::theme_system))].into_iter().map(|(t, label)| html! {
                                <button
                                    type="button"
                                    role="radio"
                                    class="topcoat-button"
                                    aria-checked={(store.theme == t).to_string()}
                                    onclick={set_theme(t, store.clone())}
                                >{ label }</button>
                            }) }
                        </div>
                    </div>

                    <div class="fn-picklist__row">
                        { icons::type_face(18) }
                        <span class="fn-grow">{ t(lang, Key::font_face) }</span>
                        <div class="fn-row fn-row--wrap" role="radiogroup" aria-label={t(lang, Key::font_face)}>
                            { for [
                                (FontFace::System, t(lang, Key::font_system)),
                                (FontFace::Skynet, t(lang, Key::font_skynet)),
                                (FontFace::Mono, t(lang, Key::font_mono)),
                                (FontFace::Serif, t(lang, Key::font_serif)),
                            ].into_iter().map(|(f, label)| {
                                let store = store.clone();
                                html! {
                                    <button
                                        type="button"
                                        role="radio"
                                        class="topcoat-button"
                                        data-font-sample={f.as_str()}
                                        aria-checked={(store.font_face == f).to_string()}
                                        onclick={Callback::from(move |_: MouseEvent| {
                                            store.dispatch(Action::SetFontFace(f));
                                        })}
                                    >{ label }</button>
                                }
                            }) }
                        </div>
                    </div>

                    <div class="fn-picklist__row">
                        { icons::type_size(18) }
                        <span class="fn-grow">{ t(lang, Key::text_size) }</span>
                        <div class="fn-row fn-row--wrap" role="radiogroup" aria-label={t(lang, Key::text_size)}>
                            { for [
                                (FontScale::Compact, t(lang, Key::size_compact)),
                                (FontScale::Standard, t(lang, Key::size_standard)),
                                (FontScale::Large, t(lang, Key::size_large)),
                                (FontScale::XLarge, t(lang, Key::size_xlarge)),
                            ].into_iter().map(|(f, label)| {
                                let store = store.clone();
                                html! {
                                    <button
                                        type="button"
                                        role="radio"
                                        class="topcoat-button"
                                        aria-checked={(store.font_scale == f).to_string()}
                                        onclick={Callback::from(move |_: MouseEvent| {
                                            store.dispatch(Action::SetFontScale(f));
                                        })}
                                    >{ label }</button>
                                }
                            }) }
                        </div>
                    </div>

                    <div class="fn-picklist__row">
                        { icons::bolt(18) }
                        <span class="fn-grow">{ t(lang, Key::connection) }</span>
                        <div class="fn-row" role="radiogroup" aria-label={t(lang, Key::connection_mode)}>
                            { for [(ConnectionMode::WebSocket, t(lang, Key::conn_live)),
                                   (ConnectionMode::Sse, t(lang, Key::conn_events)),
                                   (ConnectionMode::Polling, t(lang, Key::conn_polling))].into_iter()
                                .map(|(m, label)| html! {
                                    <button
                                        type="button"
                                        role="radio"
                                        class="topcoat-button"
                                        aria-checked={(store.mode == m).to_string()}
                                        onclick={set_mode(m, store.clone())}
                                    >{ label }</button>
                                }) }
                        </div>
                    </div>

                    <div class="fn-picklist__row">
                        { icons::ban(18) }
                        <span class="fn-grow">{ t(lang, Key::blocked_people) }</span>
                        <span class="fn-muted">
                            { format!("{} blocked", store.blocked.len()) }
                        </span>
                        <button
                            type="button"
                            class="topcoat-button"
                            disabled={offline}
                            title={offline.then_some("Unavailable offline")}
                            onclick={open(Modal::Blocked, store.clone())}
                        >{ t(lang, Key::manage) }</button>
                    </div>

                    <div class="fn-picklist__row">
                        { icons::eye(18) }
                        <span class="fn-grow">{ t(lang, Key::hidden_rooms) }</span>
                        <button
                            type="button"
                            class="topcoat-button"
                            disabled={offline}
                            title={offline.then_some("Unavailable offline")}
                            onclick={open(Modal::HiddenRooms, store.clone())}
                        >{ t(lang, Key::manage) }</button>
                    </div>
                </div>

                <h2 class="topcoat-list__header">{ t(lang, Key::ai_assistant) }</h2>
                <div class="topcoat-list__container">
                    <div class="fn-stack fn-settings__ai">
                        <p class="fn-field__help">{ t(lang, Key::ai_keys_hint) }</p>
                        <super::dialogs::AiKeysEditor />
                    </div>
                </div>

                <h2 class="topcoat-list__header">{ t(lang, Key::account_section) }</h2>
                <div class="topcoat-list__container">
                    // What this device is holding, stated plainly. A credential
                    // stored in `localStorage` that the user cannot see, copy or
                    // revoke from inside the app would be the worst version of
                    // this feature (crate::vault).
                    <div class="fn-picklist__row">
                        { icons::lock(18) }
                        <div class="fn-grow">
                            <div>{ t(lang, Key::recovery_phrase_on_device) }</div>
                            <p class="fn-field__help">
                                if let Some(v) = &stored {
                                    if v.credential.phrase().is_some() {
                                        { t(lang, Key::phrase_saved_hint) }
                                    } else {
                                        { t(lang, Key::private_key_saved_hint) }
                                    }
                                } else {
                                    { t(lang, Key::phrase_not_saved_hint) }
                                }
                            </p>
                        </div>
                        if let Some(phrase) = stored.as_ref().and_then(|v| v.credential.phrase()) {
                            <button
                                type="button"
                                class="topcoat-button"
                                onclick={copy_phrase(phrase.to_owned(), store.clone())}
                            >{ icons::copy(16) }{ " " }{ t(lang, Key::copy) }</button>
                        }
                        if stored.is_some() {
                            <button
                                type="button"
                                class="topcoat-button--danger"
                                onclick={confirm(Confirm {
                                    title: t(lang, Key::forget_phrase_title).into(),
                                    body: t(lang, Key::forget_phrase_body).into(),
                                    confirm_label: t(lang, Key::forget).into(),
                                    action: ConfirmAction::ForgetWallet,
                                }, store.clone())}
                            >{ t(lang, Key::forget) }</button>
                        }
                    </div>
                    <div class="fn-picklist__row">
                        { icons::power(18) }
                        <span class="fn-grow">{ t(lang, Key::sign_out) }</span>
                        <button
                            type="button"
                            class="topcoat-button"
                            onclick={confirm(Confirm {
                                title: t(lang, Key::sign_out_title).into(),
                                body: t(lang, Key::sign_out_body).into(),
                                confirm_label: t(lang, Key::sign_out).into(),
                                action: ConfirmAction::SignOut,
                            }, store.clone())}
                        >{ t(lang, Key::sign_out) }</button>
                    </div>
                    <div class="fn-picklist__row">
                        { icons::trash(18) }
                        <div class="fn-grow">
                            <div>{ t(lang, Key::erase_local_data) }</div>
                            <p class="fn-field__help">{ t(lang, Key::erase_local_help) }</p>
                        </div>
                        <button
                            type="button"
                            class="topcoat-button--danger"
                            onclick={confirm(Confirm {
                                title: t(lang, Key::erase_local_title).into(),
                                body: t(lang, Key::erase_local_body).into(),
                                confirm_label: t(lang, Key::erase).into(),
                                action: ConfirmAction::EraseLocalData,
                            }, store.clone())}
                        >{ t(lang, Key::erase) }</button>
                    </div>
                </div>
            </div>
        </>
    }
}

/// Screen 10 — Not found (DESIGN.md §14).
///
/// Full screen, no shell: a broken URL should not imply a working session. No
/// developer-facing copy either — "did you forget to add the page to the
/// router?" is a note to oneself, not to a user.
#[derive(Properties, PartialEq)]
pub struct NotFoundProps {
    pub on_navigate: Callback<Route>,
    /// Where the CTA goes: `/rooms` with a session, `/login` without.
    pub authenticated: bool,
}

#[function_component(NotFound)]
pub fn not_found(p: &NotFoundProps) -> Html {
    let lang = crate::state::use_store().language;
    let target = if p.authenticated {
        Route::Rooms
    } else {
        Route::Login
    };
    let onclick = {
        let on_navigate = p.on_navigate.clone();
        Callback::from(move |_: MouseEvent| on_navigate.emit(target.clone()))
    };
    html! {
        <main class="fn-404">
            <div class="fn-empty__art" aria-hidden="true">{ "⚠️" }</div>
            <p class="fn-404__code">{ "404" }</p>
            <h1 class="fn-empty__title">{ t(lang, Key::page_not_found) }</h1>
            <p class="fn-empty__desc">{ t(lang, Key::page_not_found_body) }</p>
            <button type="button" class="topcoat-button--cta" {onclick}>
                { if p.authenticated { t(lang, Key::go_to_your_rooms) } else { t(lang, Key::go_to_sign_in) } }
            </button>
        </main>
    }
}
