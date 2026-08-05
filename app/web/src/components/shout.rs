//! Shout — the paid broadcast (docs/API.md §16.1).
//!
//! Two halves, one file:
//!
//! * [`ShoutLayer`] — the banner overlay. Mounted once at the root like the
//!   burst and spotlight layers, fed by [`sync`] from anywhere. Every active
//!   shout renders as a full-width banner that powers on, burns for up to a
//!   minute, and can be dismissed. **Dismissal is per-viewer**: the ✕ hides
//!   the banner on this screen only, because the crier paid for everyone
//!   else's minute, not for yours to be revocable by a stranger.
//! * [`ShoutDialog`] — compose and pay. The browser signs a native transfer
//!   of the shout price to the server's FruitNation wallet (the same
//!   machinery as every Bank transaction, HUD included), then presents the
//!   transaction hash to `POST /api/shout`. If the POST fails the paid hash
//!   is kept, so a retry never pays twice.

use std::cell::RefCell;
use std::collections::HashSet;

use web_sys::HtmlTextAreaElement;
use yew::prelude::*;

use crate::api::shout::Shout;
use crate::format;
use crate::i18n::{t, Key};
use crate::state::use_store;

use super::icons;
use super::modal::Modal as Dialog;
use super::toast;

/// Longest shout, mirrored from the server's validator.
const MAX_TEXT_CHARS: usize = 200;

thread_local! {
    static EMIT: RefCell<Option<Callback<Vec<Shout>>>> = const { RefCell::new(None) };
}

/// Replace the layer's active set. No-op when the layer is not mounted.
pub fn sync(shouts: Vec<Shout>) {
    EMIT.with(|e| {
        if let Some(cb) = e.borrow().as_ref() {
            cb.emit(shouts);
        }
    });
}

// ------------------------------------------------------------------ layer --

#[function_component(ShoutLayer)]
pub fn shout_layer() -> Html {
    let store = use_store();
    let lang = store.language;
    let shouts = use_state(Vec::<Shout>::new);
    // Ids this viewer closed. Survives sync (the server keeps listing the
    // shout until it expires) but not a reload — which is fine, because by
    // then the shout is almost certainly ash anyway.
    let dismissed = use_mut_ref(HashSet::<String>::new);
    // Re-render tick while anything is burning, for the countdown meters.
    let now = use_state(format::now_ms);

    {
        let shouts = shouts.clone();
        use_effect_with((), move |_| {
            EMIT.with(|e| {
                *e.borrow_mut() = Some(Callback::from(move |v: Vec<Shout>| shouts.set(v)));
            });
            || EMIT.with(|e| *e.borrow_mut() = None)
        });
    }

    let any_active = shouts.iter().any(|s| s.expires_at > *now);
    {
        let now = now.clone();
        use_effect_with(any_active, move |&active| {
            let interval = active.then(|| {
                gloo_timers::callback::Interval::new(250, move || now.set(format::now_ms()))
            });
            move || drop(interval)
        });
    }

    let visible: Vec<&Shout> = shouts
        .iter()
        .filter(|s| s.expires_at > *now && !dismissed.borrow().contains(&s.id))
        .collect();
    if visible.is_empty() {
        return html! {};
    }

    html! {
        // Assertive on purpose: someone paid real money to interrupt.
        <div class="fn-shout-layer" aria-live="assertive">
            { for visible.into_iter().enumerate().map(|(i, s)| {
                let total = (s.expires_at - s.created_at).max(1) as f64;
                let left = (s.expires_at - *now).max(0) as f64;
                let pct = (left / total * 100.0).clamp(0.0, 100.0);
                let dismiss = {
                    let dismissed = dismissed.clone();
                    let shouts = shouts.clone();
                    let id = s.id.clone();
                    Callback::from(move |_: MouseEvent| {
                        dismissed.borrow_mut().insert(id.clone());
                        // Re-set the same list to force a render without the
                        // dismissed banner; the state handle is the only
                        // signal the layer re-renders on.
                        shouts.set((*shouts).clone());
                    })
                };
                html! {
                    <div class="fn-shout" key={s.id.clone()} style={format!("--i: {i}")}>
                        <span class="fn-shout__scan" aria-hidden="true"></span>
                        <img
                            class="fn-shout__herald"
                            src={crate::asset::img(store.skin, "shout-herald")}
                            alt=""
                            aria-hidden="true"
                        />
                        <div class="fn-shout__body">
                            <span class="fn-shout__from">
                                { icons::bolt(12) }
                                <span>{ &s.username }</span>
                            </span>
                            <p class="fn-shout__text">{ &s.text }</p>
                        </div>
                        <button
                            type="button"
                            class="fn-shout__close"
                            aria-label={t(lang, Key::shout_dismiss)}
                            title={t(lang, Key::shout_dismiss)}
                            onclick={dismiss}
                        >
                            { icons::close(16) }
                        </button>
                        <span
                            class="fn-shout__meter"
                            style={format!("--pct: {pct:.2}%")}
                            aria-hidden="true"
                        ></span>
                    </div>
                }
            }) }
        </div>
    }
}

// ----------------------------------------------------------------- dialog --

#[derive(Properties, PartialEq)]
pub struct ShoutDialogProps {
    pub on_close: Callback<()>,
}

#[function_component(ShoutDialog)]
pub fn shout_dialog(p: &ShoutDialogProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let text = use_state(String::new);
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    // Set once the transfer confirmed: a failed POST after a successful
    // payment must retry with this hash, never pay again.
    let paid_tx = use_state(|| Option::<String>::None);

    let price = {
        let configured = store.chain.shout_price_cro.trim();
        if configured.is_empty() {
            "10".to_owned()
        } else {
            configured.to_owned()
        }
    };
    let symbol = store
        .active_network()
        .map(|n| n.symbol.clone())
        .unwrap_or_else(|| "CRO".to_owned());

    let count = text.chars().count();

    let run = {
        let store = store.clone();
        let text = text.clone();
        let busy = busy.clone();
        let error = error.clone();
        let paid_tx = paid_tx.clone();
        let on_close = p.on_close.clone();
        let price = price.clone();
        Callback::from(move |_: ()| {
            if *busy {
                return;
            }
            let message = text.trim().to_owned();
            if message.is_empty() || message.chars().count() > MAX_TEXT_CHARS {
                error.set(Some(t(lang, Key::shout_text_invalid).to_owned()));
                return;
            }
            busy.set(true);
            error.set(None);

            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let paid_tx = paid_tx.clone();
            let on_close = on_close.clone();
            let price = price.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = pay_and_shout(&store, &message, &price, &paid_tx).await;
                match outcome {
                    Ok(shout) => {
                        toast::success(&store, t(lang, Key::shout_sent));
                        // Show our own banner immediately rather than waiting
                        // for the wake-up event to round-trip.
                        crate::actions::refresh_shouts(store.clone()).await;
                        let _ = shout;
                        on_close.emit(());
                    }
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        })
    };

    let hint = t(lang, Key::shout_hint).replace("{price}", &format!("{price} {symbol}"));
    let button_label = if paid_tx.is_some() {
        t(lang, Key::shout_retry).to_owned()
    } else {
        t(lang, Key::shout_pay).replace("{price}", &format!("{price} {symbol}"))
    };

    html! {
        <Dialog
            title={t(lang, Key::shout_title).to_owned()}
            on_close={p.on_close.clone()}
            busy={*busy}
        >
            <div class="fn-shoutdialog">
                <div class="fn-shoutdialog__hero">
                    <img src={crate::asset::img(store.skin, "shout-herald")} alt="" aria-hidden="true" />
                    <p>{ hint }</p>
                </div>
                <label class="fn-field">
                    <span class="fn-field__label">{ t(lang, Key::shout_label) }</span>
                    <textarea
                        class="topcoat-textarea fn-shoutdialog__input"
                        rows={2}
                        maxlength={MAX_TEXT_CHARS.to_string()}
                        placeholder={t(lang, Key::shout_placeholder)}
                        value={(*text).clone()}
                        data-autofocus="true"
                        disabled={*busy}
                        oninput={{
                            let text = text.clone();
                            Callback::from(move |e: InputEvent| {
                                let el: HtmlTextAreaElement = e.target_unchecked_into();
                                // Single line: the banner has one.
                                text.set(el.value().replace(['\n', '\r'], " "));
                            })
                        }}
                    />
                    <span class="fn-shoutdialog__count">{ format!("{count}/{MAX_TEXT_CHARS}") }</span>
                </label>
                if paid_tx.is_some() {
                    <p class="fn-shoutdialog__paid">{ t(lang, Key::shout_paid_note) }</p>
                }
                if let Some(e) = error.as_ref() {
                    <p class="fn-shoutdialog__error" role="alert">{ e.clone() }</p>
                }
                <div class="fn-shoutdialog__actions">
                    <button
                        type="button"
                        class="topcoat-button--large--cta fn-shoutdialog__cta"
                        disabled={*busy || count == 0 || count > MAX_TEXT_CHARS}
                        onclick={{
                            let run = run.clone();
                            Callback::from(move |_: MouseEvent| run.emit(()))
                        }}
                    >
                        { icons::bolt(16) }
                        <span>{ button_label }</span>
                    </button>
                </div>
            </div>
        </Dialog>
    }
}

/// Pay (unless already paid) and submit. Returns the created shout, or a
/// user-facing error string.
async fn pay_and_shout(
    store: &crate::state::Store,
    message: &str,
    price: &str,
    paid_tx: &UseStateHandle<Option<String>>,
) -> Result<Shout, String> {
    let tx_hash = match paid_tx.as_ref() {
        Some(hash) => hash.clone(),
        None => {
            let hash = super::bank::pay_operator(store, price).await?;
            paid_tx.set(Some(hash.clone()));
            hash
        }
    };

    store
        .client
        .shout(message, &tx_hash)
        .await
        .map_err(|e| e.user_message())
}
