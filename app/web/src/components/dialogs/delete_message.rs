//! Delete one message.

use pocketskynet_core::MessageId;
use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::state::use_store;

/// Must match `.fn-msg--dissolving` in app.css §7.
///
/// Sized against [`PROC_MS`](crate::components::burst::PROC_MS) rather than on
/// its own: the two run back to back and their sum is what a deletion costs, so
/// 240 + 260 puts the whole thing at 500ms.
const DISSOLVE_MS: u32 = 260;

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

/// Delete the attachments a deleted message was showing.
///
/// # Why the client does this and the server cannot
///
/// An attachment is its own row in `files`, with no column naming the message
/// that displayed it — the link is the URL inside the message text, and in an
/// encrypted room the server holds ciphertext and can never read it. So
/// deleting a message left its picture in the room's Files drawer and in the
/// gallery, both of which read `files` directly: the image was "deleted" and
/// still on screen. Only the client holds the key, so only the client can
/// close that gap. Same division of labour as `mentions.rs` and `media.rs`.
///
/// # Best effort, deliberately
///
/// Every failure here is ignored. `DELETE /api/files/{id}` admits the uploader
/// or a room admin, and any member may delete any *message* — so deleting
/// somebody else's photo post legitimately answers 403, and that is not an
/// error the person deleting should be shown. The message is already gone;
/// reporting a partial cleanup they cannot act on would turn a success into a
/// scary dialog.
///
/// A sealed message (`preview == None`) cleans up nothing, because this device
/// cannot read which files it named. That is the honest outcome: the
/// alternative is guessing.
async fn remove_attachments(store: &crate::state::Store, preview: Option<&str>) {
    let Some(text) = preview else {
        return;
    };
    for id in crate::api::attachment_ids_in(text) {
        let _ = store.client.delete_file(id).await;
    }
}

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
        // Captured before the task: the dialog is unmounted by the time the
        // cleanup runs, so its props are not there to read.
        let preview = p.preview.clone();
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
            let preview = preview.clone();
            wasm_bindgen_futures::spawn_local(async move {
                // 1. The machine works. Nothing has been destroyed yet, so a
                //    failure after this point still has a row to restore.
                dissolve_sleep(crate::components::burst::PROC_MS).await;
                crate::components::burst::proc_end(proc);

                // 2. Then it destroys. The dissolve and the discharge start on
                //    the same tick so the sparks look like the cause of the
                //    collapse rather than a garnish on it. The bubble is
                //    measured here, not before the readout: the list may have
                //    scrolled while it was up, and a stale position would put
                //    the blast somewhere the message never was.
                store.dispatch(crate::state::Action::Dissolve(id.clone()));
                crate::components::burst::burst_from_selector(
                    &format!("[data-id=\"{id}\"] .fn-bubble"),
                    crate::components::burst::Variant::Poof,
                    12,
                );

                // 3. Let the disintegration play before asking the server. The
                // request is what causes the row to be removed — on a LAN it
                // answers in a couple of milliseconds, and `/sync` then
                // unmounted the message before its own send-off had finished.
                // Waiting first means the effect is always seen; the delete is
                // a quarter second later, which is invisible because the row
                // already reads as gone.
                dissolve_sleep(DISSOLVE_MS).await;
                match store.client.delete_message(&id).await {
                    Ok(()) => {
                        remove_attachments(&store, preview.as_deref()).await;
                        on_deleted.emit(())
                    }
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
