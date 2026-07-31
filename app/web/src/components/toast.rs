//! The toast stack (DESIGN.md §15).
//!
//! Toasts *confirm*; they never carry the only copy of information a user
//! needs. That is why errors also appear inline at the point of failure, and
//! why an error toast never auto-dismisses — a missed error is an error that,
//! from the user's point of view, did not happen.

use gloo_timers::callback::Timeout;
use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::state::{use_store, Action, Toast, ToastKind};

use super::icons;

#[derive(Properties, PartialEq)]
struct ToastProps {
    toast: Toast,
    on_dismiss: Callback<u64>,
}

/// Matches `.fn-toast[data-leaving]` in app.css §11.
const TOAST_EXIT_MS: u32 = 140;

#[function_component(ToastItem)]
fn toast_item(p: &ToastProps) -> Html {
    let lang = use_store().language;
    let id = p.toast.id;
    let kind = p.toast.kind;
    let (role, live) = kind.live_region();
    // `.fn-toast[data-leaving]` has existed in the stylesheet since the toast
    // was written, but nothing ever set the attribute — so the exit was dead
    // CSS and every toast disappeared between two frames. Both dismissal paths
    // now route through `leave`.
    let leaving = use_state(|| false);

    let leave = {
        let leaving = leaving.clone();
        let on_dismiss = p.on_dismiss.clone();
        Callback::from(move |_: ()| {
            if *leaving {
                return;
            }
            leaving.set(true);
            let on_dismiss = on_dismiss.clone();
            wasm_bindgen_futures::spawn_local(async move {
                exit_sleep(TOAST_EXIT_MS).await;
                on_dismiss.emit(id);
            });
        })
    };

    // Arm the auto-dismiss once per toast. The timeout is dropped on unmount,
    // so a toast the user dismisses by hand does not fire a stale callback.
    {
        let leave = leave.clone();
        let ttl = kind.ttl_ms(p.toast.description.is_some());
        use_effect_with(id, move |_| {
            let handle = ttl.map(|ms| Timeout::new(ms, move || leave.emit(())));
            move || drop(handle)
        });
    }

    let dismiss = {
        let leave = leave.clone();
        Callback::from(move |_: MouseEvent| leave.emit(()))
    };

    html! {
        <div
            class={kind.class()}
            data-leaving={leaving.then(|| "true")}
            {role}
            aria-live={live}
            aria-atomic="true"
        >
            <div class="fn-toast__body">
                <p class="fn-toast__title">{ &p.toast.title }</p>
                if let Some(d) = &p.toast.description {
                    <p class="fn-toast__desc">{ d }</p>
                }
            </div>
            <button
                type="button"
                class="fn-toast__close topcoat-icon-button--quiet"
                aria-label={t(lang, Key::dismiss_toast).replace("{title}", &p.toast.title)}
                onclick={dismiss}
            >
                { icons::close(14) }
            </button>
        </div>
    }
}

/// The stack itself. At most three are ever mounted; the store evicts the
/// oldest beyond that.
#[function_component(Toasts)]
pub fn toasts() -> Html {
    let store = use_store();
    let on_dismiss = {
        let store = store.clone();
        Callback::from(move |id: u64| store.dispatch(Action::DismissToast(id)))
    };

    if store.toasts.is_empty() {
        return html! {};
    }

    html! {
        <div class="fn-toasts">
            { for store.toasts.iter().map(|t| html! {
                <ToastItem key={t.id} toast={t.clone()} on_dismiss={on_dismiss.clone()} />
            }) }
        </div>
    }
}

/// Convenience constructors, so call sites read as intent rather than as
/// enum plumbing.
pub fn success(store: &crate::state::Store, title: impl Into<String>) {
    store.dispatch(Action::Toast(ToastKind::Success, title.into(), None));
}

/// Neutral: a plain confirmation. Emerald is reserved for "encryption held",
/// so a saved rename must not toast as a success.
pub fn neutral(store: &crate::state::Store, title: impl Into<String>) {
    store.dispatch(Action::Toast(ToastKind::Neutral, title.into(), None));
}

/// Blue: something happened that the user did not initiate.
pub fn info(store: &crate::state::Store, title: impl Into<String>) {
    store.dispatch(Action::Toast(ToastKind::Info, title.into(), None));
}

pub fn warn(store: &crate::state::Store, title: impl Into<String>, desc: Option<String>) {
    store.dispatch(Action::Toast(ToastKind::Warn, title.into(), desc));
}

pub fn error(store: &crate::state::Store, title: impl Into<String>, desc: Option<String>) {
    store.dispatch(Action::Toast(ToastKind::Error, title.into(), desc));
}

#[cfg(target_arch = "wasm32")]
async fn exit_sleep(ms: u32) {
    // Reduced motion flattens the animation, so waiting for it would be a
    // stationary pause — see `modal.rs::exit_delay_ms`.
    let reduced = web_sys::window()
        .and_then(|w| {
            w.match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|m| m.matches());
    if !reduced {
        gloo_timers::future::TimeoutFuture::new(ms).await;
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn exit_sleep(_ms: u32) {}
