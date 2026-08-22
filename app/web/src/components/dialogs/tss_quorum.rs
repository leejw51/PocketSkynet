//! The app-wide TSS signing prompt (docs/CRYPTO.md §15.4).
//!
//! An MPC session deliberately retains no share files and no passphrase, so
//! every transaction signature starts here: `actions::acquire_tx_signer`
//! calls `crate::tss::request_quorum` **before the relay HUD is raised**
//! (the HUD blurs everything under it — a prompt raised beneath it reads
//! as a hung send), this host takes the pending request, and the user
//! presents `t` share files plus the passphrase — used once, in the
//! browser's ceremony worker, then dropped. Nothing is remembered between
//! signatures and nothing ever touches `localStorage`.

use pocketskynet_core::WalletAddress;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::state::use_store;
use crate::tss::{
    self, tss_quorum, tss_quorum_check, QuorumRequest, TssLoadedFile, TssQuorum, TssQuorumCheck,
};

use super::super::common::BusyButton;
use super::super::modal::Modal as Dialog;

/// Mounted once, next to the app's other global dialog hosts. Registers
/// itself as the quorum prompt while mounted.
#[function_component(TssQuorumHost)]
pub fn tss_quorum_host() -> Html {
    let store = use_store();
    let lang = store.language;

    // The request being answered. Behind a `use_mut_ref` because the
    // responder inside is a oneshot sender — not clonable, not comparable —
    // and the render only needs to know whether one is present.
    let request = use_mut_ref(|| Option::<QuorumRequest>::None);
    let address = use_state(|| Option::<WalletAddress>::None);
    let files = use_state(Vec::<TssLoadedFile>::new);
    let passphrase = use_state(String::new);
    let error = use_state(|| Option::<String>::None);

    {
        // Register for the lifetime of the mount. The poke callback pulls
        // the pending request out of `crate::tss` and opens the dialog.
        let request = request.clone();
        let address = address.clone();
        let files = files.clone();
        let passphrase = passphrase.clone();
        let error = error.clone();
        use_effect_with((), move |_| {
            let poke = Callback::from(move |_: ()| {
                if let Some(req) = tss::take_pending_request() {
                    address.set(Some(req.address.clone()));
                    files.set(Vec::new());
                    passphrase.set(String::new());
                    error.set(None);
                    *request.borrow_mut() = Some(req);
                }
            });
            tss::register_prompt_host(Some(poke));
            || tss::register_prompt_host(None)
        });
    }

    let Some(expected) = (*address).clone() else {
        return Html::default();
    };

    let finish = {
        let request = request.clone();
        let address = address.clone();
        let files = files.clone();
        let passphrase = passphrase.clone();
        let error = error.clone();
        move |quorum: Option<TssQuorum>| {
            if let Some(req) = request.borrow_mut().take() {
                req.resolve(quorum);
            }
            address.set(None);
            files.set(Vec::new());
            passphrase.set(String::new());
            error.set(None);
        }
    };

    let on_cancel = {
        let finish = finish.clone();
        Callback::from(move |_: ()| finish(None))
    };
    let on_cancel_click = {
        let finish = finish.clone();
        Callback::from(move |_: MouseEvent| finish(None))
    };

    let on_files = {
        let files = files.clone();
        let error = error.clone();
        Callback::from(move |e: Event| {
            let Some(input) = e.target_dyn_into::<HtmlInputElement>() else {
                return;
            };
            let Some(list) = input.files() else {
                return;
            };
            error.set(None);
            let picked: Vec<web_sys::File> =
                (0..list.length()).filter_map(|i| list.get(i)).collect();
            let files = files.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let pool = tss::read_share_files((*files).clone(), picked).await;
                files.set(pool);
            });
            input.set_value("");
        })
    };

    let on_clear = {
        let files = files.clone();
        let error = error.clone();
        Callback::from(move |_: MouseEvent| {
            files.set(Vec::new());
            error.set(None);
        })
    };

    let on_passphrase = {
        let passphrase = passphrase.clone();
        let error = error.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                passphrase.set(el.value());
                error.set(None);
            }
        })
    };

    let on_sign = {
        let files = files.clone();
        let passphrase = passphrase.clone();
        let error = error.clone();
        let expected = expected.clone();
        let finish = finish.clone();
        Callback::from(move |_: MouseEvent| {
            let Some((address, shares)) = tss_quorum(&files) else {
                error.set(Some(t(lang, Key::tss_select_first).into()));
                return;
            };
            if address != expected {
                error.set(Some(t(lang, Key::tss_wrong_wallet).into()));
                return;
            }
            if passphrase.trim().is_empty() {
                error.set(Some(t(lang, Key::tss_enter_passphrase).into()));
                return;
            }
            finish(Some(TssQuorum {
                shares,
                passphrase: (*passphrase).clone(),
            }));
        })
    };

    // The same one-verdict status line the login picker renders, minus the
    // artwork: this dialog interrupts a send, so it stays lean.
    let status = match tss_quorum_check(&files) {
        TssQuorumCheck::Empty => html! {
            <p class="fn-field__help">{ t(lang, Key::tss_files_hint) }</p>
        },
        TssQuorumCheck::Mismatch => html! {
            <p class="fn-login__error" role="alert">{ t(lang, Key::tss_files_mismatch) }</p>
        },
        TssQuorumCheck::NeedMore(more) => html! {
            <p class="fn-field__help">
                { t(lang, Key::tss_need_more).replace("{more}", &more.to_string()) }
            </p>
        },
        TssQuorumCheck::Ready { have, need, .. } => html! {
            <p class="fn-tss-quorum-ok">
                { t(lang, Key::tss_files_loaded)
                    .replace("{have}", &have.to_string())
                    .replace("{need}", &need.to_string()) }
            </p>
        },
    };

    let ready = tss_quorum(&files)
        .map(|(a, _)| a == expected)
        .unwrap_or(false)
        && !passphrase.trim().is_empty();

    html! {
        <Dialog
            title={t(lang, Key::tss_sign_title).to_string()}
            busy={false}
            on_close={on_cancel}
            footer={Some(html! {
                <>
                    <button type="button" class="topcoat-button" onclick={on_cancel_click}>
                        { t(lang, Key::cancel) }
                    </button>
                    <BusyButton
                        label={t(lang, Key::tss_sign_button).to_string()}
                        busy={false}
                        disabled={!ready}
                        onclick={on_sign}
                    />
                </>
            })}
        >
            <p>{ t(lang, Key::tss_sign_body) }</p>
            <p class="fn-mono">{ expected.as_str() }</p>
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::tss_files_label) }</span>
                <label class="topcoat-button fn-tss-pick">
                    { t(lang, Key::tss_add_files) }
                    <input
                        type="file"
                        accept="application/json,.json"
                        multiple=true
                        class="fn-visually-hidden"
                        onchange={on_files}
                    />
                </label>
                if !files.is_empty() {
                    <ul class="fn-tss-files">
                        { for files.iter().map(|f| html! {
                            <li class="fn-tss-file" data-share={f.header.is_some().to_string()}>
                                <span class="fn-grow">{ f.name.clone() }</span>
                                if let Some(h) = &f.header {
                                    <span class="fn-tss-file__shape">{ h.shape() }</span>
                                } else {
                                    <span class="fn-tss-file__bad">
                                        { t(lang, match tss::legacy_share_version(&f.value) {
                                                Some(_) => Key::tss_files_legacy,
                                                None => Key::tss_files_invalid,
                                            })
                                            .replace("{name}", &f.name) }
                                    </span>
                                }
                            </li>
                        }) }
                    </ul>
                    <button type="button" class="topcoat-button" onclick={on_clear}>
                        { t(lang, Key::tss_clear_files) }
                    </button>
                }
                { status }
            </div>
            <div class="fn-field">
                <label class="fn-field__label" for="tss-quorum-pass">
                    { t(lang, Key::tss_passphrase) }
                </label>
                <input
                    id="tss-quorum-pass"
                    class="topcoat-text-input"
                    type="password"
                    autocomplete="off"
                    value={(*passphrase).clone()}
                    oninput={on_passphrase}
                />
                <p class="fn-field__help">{ t(lang, Key::tss_sign_hint) }</p>
            </div>
            if let Some(e) = &*error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }
        </Dialog>
    }
}
