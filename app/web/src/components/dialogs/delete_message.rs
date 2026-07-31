//! Delete one message.

use pocketskynet_core::MessageId;
use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::state::use_store;

/// Must match `.fn-msg--dissolving` in app.css §7.
const DISSOLVE_MS: u32 = 460;

#[cfg(target_arch = "wasm32")]
async fn dissolve_sleep(ms: u32) {
    // Reduced motion has no animation to wait for, and a deletion that takes
    // half a second longer for no visible reason is just latency.
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
async fn dissolve_sleep(_ms: u32) {}

/// Delete one message. Any member may delete any message — the copy says so
/// rather than implying it is only your own.
#[derive(Properties, PartialEq)]
pub struct DeleteMessageProps {
    pub message_id: MessageId,
    /// The message's readable text, quoted in the dialog so what is about to
    /// be destroyed is on screen while deciding. `None` for sealed content.
    #[prop_or_default]
    pub preview: Option<String>,
    pub on_close: Callback<()>,
    pub on_deleted: Callback<()>,
}

#[function_component(DeleteMessage)]
pub fn delete_message(p: &DeleteMessageProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);

    let confirm = {
        let store = store.clone();
        let busy = busy.clone();
        let error = error.clone();
        let id = p.message_id.clone();
        let on_deleted = p.on_deleted.clone();
        Callback::from(move |_: ()| {
            busy.set(true);
            // The readout first: the machine acquires the target and purges the
            // record before anything visibly happens to the row. The bubble
            // stays put and legible throughout, which is what makes the three
            // seconds read as deliberation rather than lag.
            let proc =
                crate::components::burst::proc_start(crate::components::burst::Variant::Poof);
            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let id = id.clone();
            let on_deleted = on_deleted.clone();
            wasm_bindgen_futures::spawn_local(async move {
                // 1. The machine works. Nothing has been destroyed yet, so a
                //    failure after this point still has a row to restore.
                dissolve_sleep(crate::components::burst::PROC_MS).await;
                crate::components::burst::proc_end(proc);

                // 2. Then it destroys. The dissolve and the discharge start on
                //    the same tick so the sparks look like the cause of the
                //    collapse rather than a garnish on it. The bubble is
                //    measured here, not three seconds ago: the list may have
                //    scrolled while the readout was up, and a stale position
                //    would put the blast somewhere the message never was.
                store.dispatch(crate::state::Action::Dissolve(id.clone()));
                crate::components::burst::burst_from_selector(
                    &format!("[data-id=\"{id}\"] .fn-bubble"),
                    crate::components::burst::Variant::Poof,
                    12,
                );

                // 3. Let the disintegration play before asking the server. The
                // request is what causes the row to be removed — on a LAN it
                // answers in a couple of milliseconds, and `/sync` then
                // unmounted the message about 450ms before its own send-off
                // had finished. Waiting first means the effect is always seen;
                // the delete is a few hundred milliseconds later, which is
                // invisible because the row already reads as gone.
                dissolve_sleep(DISSOLVE_MS).await;
                match store.client.delete_message(&id).await {
                    Ok(()) => on_deleted.emit(()),
                    Err(e) => {
                        // It is still there. Stop pretending otherwise.
                        store.dispatch(crate::state::Action::UndoDissolve(id.clone()));
                        error.set(Some(e.user_message()));
                    }
                }
                busy.set(false);
            });
        })
    };

    html! {
        <super::super::modal::ConfirmDialog
            title={t(lang, Key::delete_message_title)}
            body={t(lang, Key::delete_message_body)}
            quote={p.preview.clone()}
            confirm_label={t(lang, Key::delete)}
            busy={*busy}
            error={(*error).clone()}
            on_confirm={confirm}
            on_cancel={{
                let on_close = p.on_close.clone();
                Callback::from(move |_: ()| on_close.emit(()))
            }}
        />
    }
}
