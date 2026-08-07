//! The modal shell and the destructive-confirmation dialog.
//!
//! `window.confirm` appears nowhere in this client. Every confirmation names
//! the object and states the consequence, and its buttons are labelled with the
//! verb ("Delete room", not "OK") — DESIGN.md §15.
//!
//! Focus handling: focus moves into the dialog on open, is trapped by a
//! `keydown` handler on the panel, and returns to whatever was focused before.
//! `Esc` and a backdrop click both close — **except** while a mutation is in
//! flight, when both are ignored and the close button is disabled, because
//! dismissing a dialog mid-write leaves the user with no idea whether it
//! happened.

use wasm_bindgen::JsCast;
use web_sys::HtmlElement;
use yew::prelude::*;

use super::common::{BusyButton, Spinner};
use super::icons;
use crate::i18n::{t, Key};

#[derive(Properties, PartialEq)]
pub struct ModalProps {
    pub title: String,
    #[prop_or_default]
    pub description: Option<String>,
    pub on_close: Callback<()>,
    #[prop_or_default]
    pub wide: bool,
    #[prop_or_default]
    pub danger: bool,
    /// While `true`, `Esc`, the backdrop and the close button are inert.
    #[prop_or_default]
    pub busy: bool,
    pub children: Children,
    /// Rendered into `.fn-modal__foot`. `Option` rather than `Children` so the
    /// footer element is omitted entirely — an empty bordered strip reads as a
    /// rendering bug.
    #[prop_or_default]
    pub footer: Option<Html>,
}

/// How long the exit animation runs before the dialog is actually unmounted.
/// Must match `.fn-modal-backdrop[data-closing]` in app.css §9 — nothing
/// enforces it, so the two are documented as one timeline.
///
/// Shorter than the entrance on purpose. An entrance is the interface
/// answering you and can afford to be expressive; an exit is you having
/// already decided, and every millisecond of it is a millisecond of waiting.
const EXIT_MS: u32 = 140;

#[cfg(target_arch = "wasm32")]
async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep(_ms: u32) {}

/// The exit delay, or none at all under `prefers-reduced-motion`.
///
/// §17 flattens the animation itself to zero duration, but the *timer* is
/// Rust's and would keep the dialog mounted for 140ms with nothing happening
/// — which is exactly the lag the setting exists to remove. Someone who asked
/// for no motion asked for the dialog to be gone, not for a stationary one.
fn exit_delay_ms() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        let reduced = web_sys::window()
            .and_then(|w| {
                w.match_media("(prefers-reduced-motion: reduce)")
                    .ok()
                    .flatten()
            })
            .is_some_and(|m| m.matches());
        if reduced {
            return 0;
        }
    }
    EXIT_MS
}

#[function_component(Modal)]
pub fn modal(p: &ModalProps) -> Html {
    let lang = crate::state::use_store().language;
    let panel = use_node_ref();
    // The dialog animates *out* before it unmounts. Without this it vanished
    // between two frames while the entrance was a considered spring — and an
    // interface that arrives gracefully and leaves by disappearing reads as
    // broken rather than fast.
    let closing = use_state(|| false);

    // Every close path routes through here: Esc, the backdrop, the close
    // button. Guarded against re-entry so a second Esc during the exit does
    // not queue a second unmount.
    let request_close = {
        let closing = closing.clone();
        let on_close = p.on_close.clone();
        Callback::from(move |_: ()| {
            if *closing {
                return;
            }
            closing.set(true);
            let on_close = on_close.clone();
            wasm_bindgen_futures::spawn_local(async move {
                sleep(exit_delay_ms()).await;
                on_close.emit(());
            });
        })
    };

    // Move focus into the dialog once, on mount, and restore it on unmount.
    {
        let panel = panel.clone();
        use_effect_with((), move |_| {
            let previous = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.active_element())
                .and_then(|e| e.dyn_into::<HtmlElement>().ok());

            if let Some(el) = panel.cast::<HtmlElement>() {
                // The first interactive element, or the panel itself for a
                // confirm (where landing on a destructive button would be a
                // trap for anyone who types Enter reflexively).
                let target = el
                    .query_selector("[data-autofocus]")
                    .ok()
                    .flatten()
                    .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                    .unwrap_or(el);
                let _ = target.focus();
            }
            move || {
                if let Some(el) = previous {
                    let _ = el.focus();
                }
            }
        });
    }

    let onkeydown = {
        let request_close = request_close.clone();
        let panel = panel.clone();
        let busy = p.busy;
        Callback::from(move |e: KeyboardEvent| {
            match e.key().as_str() {
                "Escape" if !busy => {
                    e.stop_propagation();
                    request_close.emit(());
                }
                "Tab" => {
                    // A hand-rolled trap: find the focusable elements inside the
                    // panel and wrap the ends. Without this, Tab walks out of
                    // the dialog into the page behind it, which is invisible to
                    // a sighted user but completely disorienting otherwise.
                    let Some(root) = panel.cast::<web_sys::Element>() else {
                        return;
                    };
                    let Ok(list) = root.query_selector_all(FOCUSABLE) else {
                        return;
                    };
                    let n = list.length();
                    if n == 0 {
                        return;
                    }
                    let first = list.item(0).and_then(|n| n.dyn_into::<HtmlElement>().ok());
                    let last = list
                        .item(n - 1)
                        .and_then(|n| n.dyn_into::<HtmlElement>().ok());
                    let active = web_sys::window()
                        .and_then(|w| w.document())
                        .and_then(|d| d.active_element());

                    let (Some(first), Some(last), Some(active)) = (first, last, active) else {
                        return;
                    };
                    let active: &web_sys::Node = active.as_ref();
                    if e.shift_key() && active.is_same_node(Some(first.as_ref())) {
                        e.prevent_default();
                        let _ = last.focus();
                    } else if !e.shift_key() && active.is_same_node(Some(last.as_ref())) {
                        e.prevent_default();
                        let _ = first.focus();
                    }
                }
                _ => {}
            }
        })
    };

    let onbackdrop = {
        let request_close = request_close.clone();
        let busy = p.busy;
        Callback::from(move |e: MouseEvent| {
            // Only a click on the backdrop *itself* — a click that started
            // inside the panel and drifted out must not close it.
            let is_backdrop = e
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .is_some_and(|el| el.class_list().contains("fn-modal-backdrop"));
            if is_backdrop && !busy {
                request_close.emit(());
            }
        })
    };

    let close = {
        let request_close = request_close.clone();
        Callback::from(move |_: MouseEvent| request_close.emit(()))
    };

    let mut class = classes!("fn-modal");
    if p.wide {
        class.push("fn-modal--wide");
    }
    if p.danger {
        class.push("fn-modal--danger");
    }

    html! {
        <div
            class="fn-modal-backdrop"
            data-closing={closing.then(|| "true")}
            onclick={onbackdrop}
        >
            <div
                ref={panel}
                {class}
                role="dialog"
                aria-modal="true"
                aria-labelledby="fn-modal-title"
                aria-describedby={p.description.as_ref().map(|_| "fn-modal-desc")}
                tabindex="-1"
                {onkeydown}
            >
                <header class="fn-modal__head">
                    // The danger sigil is the dialog's focal point — one red
                    // mark instead of red sprayed across title and buttons.
                    if p.danger {
                        <div class="fn-modal__sigil" aria-hidden="true">
                            { icons::warn(20) }
                        </div>
                    }
                    <div>
                        <h2 class="fn-modal__title" id="fn-modal-title">{ &p.title }</h2>
                        if let Some(d) = &p.description {
                            <p class="fn-modal__desc" id="fn-modal-desc">{ d }</p>
                        }
                    </div>
                    <button
                        type="button"
                        class="fn-modal__close topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::close)}
                        disabled={p.busy}
                        onclick={close}
                    >
                        { icons::close(18) }
                    </button>
                </header>
                <div class="fn-modal__body fn-scroll">{ for p.children.iter() }</div>
                if let Some(f) = &p.footer {
                    <footer class="fn-modal__foot">{ f.clone() }</footer>
                }
            </div>
        </div>
    }
}

/// Everything that can hold focus, in DOM order. `[tabindex="-1"]` is excluded
/// deliberately: the panel itself matches it and would otherwise become a stop.
const FOCUSABLE: &str = "button:not([disabled]), [href], input:not([disabled]), \
     select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex=\"-1\"])";

#[derive(Properties, PartialEq)]
pub struct ConfirmProps {
    /// Phrased as a question: "Delete room?".
    pub title: String,
    /// The irreversible fact, stated plainly.
    pub body: String,
    /// The verb, e.g. "Delete room".
    pub confirm_label: String,
    pub on_confirm: Callback<()>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub busy: bool,
    #[prop_or_default]
    pub error: Option<String>,
    /// What the action will destroy, quoted — deleting is easier to confirm
    /// (and to abort) when the dialog shows the thing itself.
    #[prop_or_default]
    pub quote: Option<String>,
    /// A second, non-destructive way forward, e.g. "Just leave" beside
    /// "Destroy room". Present only when both are real answers to the same
    /// question; a dialog with two verbs and no clear default is a dialog
    /// people dismiss.
    #[prop_or_default]
    pub alternative_label: Option<String>,
    #[prop_or_default]
    pub on_alternative: Option<Callback<()>>,
    /// A phrase that must be typed before the destructive verb is enabled.
    /// See [`crate::state::Confirm::challenge`] for when that is warranted.
    #[prop_or_default]
    pub challenge: Option<String>,
}

/// Does what was typed satisfy the challenge?
///
/// Trimmed and case-folded: the gate is there to make the action deliberate,
/// not to test typing. Someone whose keyboard capitalised the first letter, or
/// who picked up a trailing space, has still spelled the words out — and a
/// button that stays dead with the right phrase visibly in the box reads as
/// broken, which teaches people to distrust the next one.
fn challenge_met(phrase: &str, typed: &str) -> bool {
    typed.trim().to_lowercase() == phrase.trim().to_lowercase()
}

/// A destructive confirmation.
///
/// The footer is always `[secondary] [primary]` with the primary last, and
/// `Cancel` comes first with a gap — a destructive button adjacent to its
/// harmless neighbour is how people delete rooms by accident (DESIGN.md §17).
#[function_component(ConfirmDialog)]
pub fn confirm_dialog(p: &ConfirmProps) -> Html {
    let lang = crate::state::use_store().language;
    let typed = use_state(String::new);
    // No challenge is "already satisfied" — every existing dialog takes this
    // branch and is unaffected.
    let unlocked = p
        .challenge
        .as_ref()
        .is_none_or(|phrase| challenge_met(phrase, &typed));
    let cancel = {
        let on_cancel = p.on_cancel.clone();
        Callback::from(move |_: MouseEvent| on_cancel.emit(()))
    };
    let confirm = {
        let on_confirm = p.on_confirm.clone();
        Callback::from(move |_: MouseEvent| on_confirm.emit(()))
    };
    let close = {
        let on_cancel = p.on_cancel.clone();
        Callback::from(move |_: ()| on_cancel.emit(()))
    };
    // Between Cancel and the destructive verb, and styled as neither: it is a
    // real action, so not `--quiet` like a menu item, and it is not the one
    // being warned about, so nothing red about it.
    let alternative = match (&p.alternative_label, &p.on_alternative) {
        (Some(label), Some(cb)) => {
            let cb = cb.clone();
            let onclick = Callback::from(move |_: MouseEvent| cb.emit(()));
            html! {
                <button type="button" class="topcoat-button" disabled={p.busy} {onclick}>
                    { label }
                </button>
            }
        }
        _ => Html::default(),
    };

    html! {
        <Modal
            title={p.title.clone()}
            description={p.body.clone()}
            danger=true
            busy={p.busy}
            on_close={close}
            footer={
                Some(html! {
                    <>
                        <button type="button" class="topcoat-button" disabled={p.busy} onclick={cancel}>
                            { t(lang, Key::cancel) }
                        </button>
                        { alternative }
                        <BusyButton
                            label={p.confirm_label.clone()}
                            // `--cta` carries the family geometry (radius,
                            // height, focus ring); `--cta-danger` only
                            // recolours it. Alone it was a colour with no
                            // shape — square next to its rounded neighbour.
                            class="topcoat-button--cta topcoat-button--cta-danger"
                            busy={p.busy}
                            disabled={!unlocked}
                            onclick={confirm}
                        />
                    </>
                })
            }
        >
            if let Some(q) = &p.quote {
                <blockquote class="fn-modal__quote">{ q }</blockquote>
            }
            if let Some(phrase) = &p.challenge {
                <div class="fn-field">
                    <label class="fn-field__label" for="fn-confirm-challenge">
                        { t(lang, Key::type_to_confirm).replace("{phrase}", phrase) }
                    </label>
                    <input
                        id="fn-confirm-challenge"
                        class="topcoat-text-input"
                        type="text"
                        // The phrase must come from the user, so nothing may
                        // offer to supply it: no history, no autocorrect
                        // rewriting it into something that then fails to match,
                        // and no capitalisation the comparison has to forgive.
                        autocomplete="off"
                        autocapitalize="none"
                        autocorrect="off"
                        spellcheck="false"
                        // Safe to focus, unlike a destructive button: a reflex
                        // Enter here types nothing and submits nothing.
                        data-autofocus="true"
                        disabled={p.busy}
                        value={(*typed).clone()}
                        oninput={{
                            let typed = typed.clone();
                            Callback::from(move |e: InputEvent| {
                                if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                    typed.set(el.value());
                                }
                            })
                        }}
                        onkeydown={{
                            let on_confirm = p.on_confirm.clone();
                            let busy = p.busy;
                            Callback::from(move |e: KeyboardEvent| {
                                // Enter submits, but only once the phrase is
                                // right — otherwise this is the reflex Enter
                                // the whole gate exists to stop.
                                if e.key() == "Enter" && unlocked && !busy {
                                    e.prevent_default();
                                    on_confirm.emit(());
                                }
                            })
                        }}
                    />
                </div>
            }
            if let Some(e) = &p.error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }
            if p.busy {
                <p class="fn-muted"><Spinner />{ " " }{ t(lang, Key::working) }</p>
            }
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phrase_has_to_be_spelled_out() {
        assert!(challenge_met("remove all", "remove all"));
        // Nothing shorter, and nothing that merely contains it: the gate is
        // there so the words have to be produced, not approached.
        assert!(!challenge_met("remove all", ""));
        assert!(!challenge_met("remove all", "remove"));
        assert!(!challenge_met("remove all", "remove al"));
        assert!(!challenge_met("remove all", "please remove all rooms"));
        assert!(!challenge_met("remove all", "delete all"));
    }

    #[test]
    fn case_and_stray_space_are_forgiven_because_they_are_not_the_point() {
        // A phone that capitalised the first letter, or a paste that brought a
        // space, has still spelled the words out. A button that stays dead
        // with the right phrase visibly in the box teaches people that the
        // interface is broken — and the next gate gets less attention, not
        // more.
        assert!(challenge_met("remove all", "Remove All"));
        assert!(challenge_met("remove all", "  remove all  "));
        assert!(challenge_met("remove all", "REMOVE ALL"));
    }

    #[test]
    fn a_localised_phrase_is_compared_the_same_way() {
        // The Korean phrase has no case to fold, which must not make it
        // stricter or looser than the English one.
        assert!(challenge_met("전체 삭제", "전체 삭제"));
        assert!(challenge_met("전체 삭제", " 전체 삭제 "));
        assert!(!challenge_met("전체 삭제", "전체"));
    }
}
