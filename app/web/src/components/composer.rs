//! The composer (DESIGN.md §7.7).
//!
//! Enter sends, Shift+Enter breaks a line, and the hint under the field says
//! so — a keyboard contract nobody can guess is a keyboard contract nobody
//! uses. The placeholder names the room ("Message Harvest planning") rather
//! than saying "Type a message…", so a mis-navigated send is obvious before it
//! happens rather than after.
//!
//! When an encrypted post cannot succeed the composer is `data-locked="true"`:
//! dimmed and inert, with the placeholder replaced by the action that would
//! unblock it. That is the first of the two corrections this client makes to
//! the reference, where the same state silently swallows every send.
//!
//! The placeholder is reason-specific on purpose — see [`PostBlock`]. Telling
//! someone whose keys are merely not on this device to "rotate the room key"
//! sends them after an admin action that would not have helped.

use web_sys::HtmlTextAreaElement;
use yew::prelude::*;

use crate::state::PostBlock;

use super::common::Popover;
use super::icons;
use crate::i18n::{t, Key};

#[derive(Properties, PartialEq)]
pub struct ComposerProps {
    pub room_name: String,
    pub on_send: Callback<String>,
    /// Emitted at most once every couple of seconds while typing.
    pub on_typing: Callback<()>,
    /// Set when an encrypted post cannot succeed. The field goes inert and the
    /// placeholder names the action that would unblock it — which differs by
    /// reason, so this carries the reason rather than a bare flag.
    #[prop_or_default]
    pub blocked: Option<PostBlock>,
    /// Offline: still enabled, because sends queue and flush on reconnect.
    #[prop_or_default]
    pub offline: bool,
    #[prop_or_default]
    pub on_open_picker: Callback<()>,
    /// Opens the AI assistant dialog for this room.
    #[prop_or_default]
    pub on_open_assistant: Callback<()>,
    /// A file was picked: its name, bytes, and whatever was in the field at the
    /// time, which becomes the caption. The composer reads the bytes so the
    /// parent never has to touch `web_sys::File`.
    #[prop_or_default]
    pub on_attach: Callback<(String, Vec<u8>, String)>,
    /// Opens the Files drawer for this room.
    #[prop_or_default]
    pub on_open_files: Callback<()>,
}

#[function_component(Composer)]
pub fn composer(p: &ComposerProps) -> Html {
    let lang = crate::state::use_store().language;
    let text = use_state(String::new);
    let area = use_node_ref();
    let send_btn = use_node_ref();
    let file_input = use_node_ref();
    let last_typing = use_mut_ref(|| 0f64);

    let send = {
        let text = text.clone();
        let on_send = p.on_send.clone();
        let send_btn = send_btn.clone();
        Callback::from(move |_: ()| {
            let body = text.trim().to_owned();
            if body.is_empty() {
                return;
            }
            // The composer clears now: as far as the person is concerned the
            // message has left, and the readout accounts for the pause.
            text.set(String::new());

            // Order is the whole point — process, discharge, *then* the message
            // exists. Sending first put the bubble on screen ahead of the
            // effect that was supposed to have produced it, which read as an
            // unrelated animation playing over a done deal.
            //
            // The cost is honest: the send really is held for PROC_MS, so the
            // message reaches the room that much later. That is the trade the
            // sequence asks for, and it is why PROC_MS is budgeted rather than
            // chosen — press to bubble is PROC_MS + SPARK_LEAD_MS, and on a LAN
            // the request itself is noise next to it.
            let proc = super::burst::proc_start(super::burst::Variant::Pop);
            let send_btn = send_btn.clone();
            let on_send = on_send.clone();
            wasm_bindgen_futures::spawn_local(async move {
                // 1. PROCESSING — the machine deliberates.
                gloo_timers::future::TimeoutFuture::new(super::burst::PROC_MS).await;
                super::burst::proc_end(proc);

                // 2. DISCHARGE. Measured now rather than before the wait: the
                //    cleared text reflowed the composer, so a position read
                //    earlier would be one the button has since left.
                super::burst::burst_from_node(&send_btn, super::burst::Variant::Pop, 12);

                // 3. The message blooms *out of* the sparks rather than after
                //    them. The burst runs for over a second, so waiting for it
                //    to finish would read as two unrelated events; landing
                //    while the streaks are still in the air makes the discharge
                //    look like the thing that produced the bubble.
                gloo_timers::future::TimeoutFuture::new(super::burst::SPARK_LEAD_MS).await;
                on_send.emit(body);
            });
        })
    };

    let oninput = {
        let text = text.clone();
        let on_typing = p.on_typing.clone();
        let last_typing = last_typing.clone();
        Callback::from(move |e: InputEvent| {
            let Some(el) = e.target_dyn_into::<HtmlTextAreaElement>() else {
                return;
            };
            text.set(el.value());

            // Self-throttle on top of the server's 1/s cap. A typing frame per
            // keystroke is pure noise and would trip the relay throttle, which
            // drops silently — so the indicator would get *worse*, not better.
            let now = js_sys::Date::now();
            if now - *last_typing.borrow() > crate::realtime::TYPING_THROTTLE_MS {
                *last_typing.borrow_mut() = now;
                on_typing.emit(());
            }
        })
    };

    let onkeydown = {
        let send = send.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" && !e.shift_key() {
                e.prevent_default();
                send.emit(());
            }
        })
    };

    let click_send = {
        let send = send.clone();
        Callback::from(move |_: MouseEvent| send.emit(()))
    };

    // Read the picked file here rather than handing a `web_sys::File` upward:
    // the bytes are what every caller wants, and keeping the DOM type in this
    // one place means the rest of the app never grows a `web-sys` dependency
    // on the file APIs.
    //
    // `Blob::array_buffer` rather than `FileReader`: same result, no event
    // plumbing, one fewer enabled web-sys feature.
    let onpick = {
        let on_attach = p.on_attach.clone();
        let file_input = file_input.clone();
        let text = text.clone();
        Callback::from(move |_: Event| {
            let Some(input) = file_input.cast::<web_sys::HtmlInputElement>() else {
                return;
            };
            let Some(file) = input.files().and_then(|list| list.get(0)) else {
                return;
            };
            let name = file.name();
            // Clear the input *now*, so picking the same file twice in a row
            // still fires `change` the second time.
            input.set_value("");

            // Whatever is in the field becomes the caption, and the field
            // clears. This is the only way to tag an attachment, and making it
            // the composer rather than a second dialog is deliberate: type
            // "#q3 #finance", attach, done. A modal asking for tags after every
            // pick would be answered with an empty box nine times in ten.
            let caption = text.trim().to_owned();
            text.set(String::new());

            let on_attach = on_attach.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let blob: web_sys::Blob = file.into();
                let Ok(buffer) = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await
                else {
                    return;
                };
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                on_attach.emit((name, bytes, caption));
            });
        })
    };

    let locked = p.blocked.is_some();
    let placeholder = match p.blocked {
        Some(reason) => reason.composer_hint().to_owned(),
        None => t(lang, Key::message_placeholder).replace("{room}", &p.room_name),
    };

    html! {
        <div class="fn-composer" data-locked={locked.to_string()}>
            <button
                type="button"
                class="topcoat-icon-button--quiet"
                aria-label={t(lang, Key::insert_emoticon)}
                disabled={locked}
                onclick={{
                    let cb = p.on_open_picker.clone();
                    Callback::from(move |_: MouseEvent| cb.emit(()))
                }}
            >
                { icons::smile(18) }
            </button>
            <button
                type="button"
                class="topcoat-icon-button--quiet fn-composer__ai"
                aria-label={t(lang, Key::open_ai_assistant)}
                title={t(lang, Key::ai_assistant)}
                disabled={locked}
                onclick={{
                    let cb = p.on_open_assistant.clone();
                    Callback::from(move |_: MouseEvent| cb.emit(()))
                }}
            >
                { icons::spark(18) }
            </button>
            <button
                type="button"
                class="topcoat-icon-button--quiet fn-composer__attach"
                aria-label={t(lang, Key::attach_file)}
                title={t(lang, Key::attach_file)}
                disabled={locked}
                onclick={{
                    // The real <input type="file"> is hidden and clicked from
                    // here: its native button cannot be restyled, and this is
                    // the only way to keep one control geometry across the row.
                    let file_input = file_input.clone();
                    Callback::from(move |_: MouseEvent| {
                        if let Some(el) = file_input.cast::<web_sys::HtmlInputElement>() {
                            el.click();
                        }
                    })
                }}
            >
                { icons::paperclip(18) }
            </button>
            <button
                type="button"
                class="topcoat-icon-button--quiet fn-composer__files"
                aria-label={t(lang, Key::open_files)}
                title={t(lang, Key::files_title)}
                onclick={{
                    let cb = p.on_open_files.clone();
                    Callback::from(move |_: MouseEvent| cb.emit(()))
                }}
            >
                { icons::files(18) }
            </button>
            <input
                ref={file_input.clone()}
                type="file"
                class="fn-sr-only"
                tabindex="-1"
                aria-hidden="true"
                onchange={onpick}
            />
            <textarea
                ref={area}
                class="fn-composer__input topcoat-textarea"
                rows="1"
                aria-label={placeholder.clone()}
                {placeholder}
                disabled={locked}
                value={(*text).clone()}
                {oninput}
                {onkeydown}
            />
            <button
                ref={send_btn.clone()}
                type="button"
                class="topcoat-button--cta"
                disabled={locked || text.trim().is_empty()}
                onclick={click_send}
            >
                { icons::send(16) }
                { t(lang, Key::send_button) }
            </button>
            <p class="fn-composer__hint">
                if p.offline {
                    { t(lang, Key::offline_queue_note) }
                } else {
                    { t(lang, Key::enter_to_send) }
                }
            </p>
        </div>
    }
}

/// The emoticon picker: an 8-column grid with a category tab row.
///
/// Traps focus and returns it to the trigger on `Esc`, like every other overlay
/// in this client.
#[derive(Properties, PartialEq)]
pub struct PickerProps {
    pub on_pick: Callback<String>,
    pub on_close: Callback<()>,
    /// Driven by the parent's `Option<target>`. The component is rendered
    /// unconditionally so `Popover` can animate it out; see `common.rs`.
    pub open: bool,
}

/// A deliberately small, curated set. An exhaustive Unicode emoji table would
/// be tens of kilobytes of `.wasm` for a feature that is used with about a
/// dozen glyphs in practice.
const EMOTICONS: &[(&str, &[&str])] = &[
    (
        "Ident",
        &[
            "🍎", "🍊", "🍋", "🍌", "🍇", "🍓", "🫐", "🍒", "🍑", "🥝", "🍉", "🍍", "🥭", "🍐",
            "🥥", "🍅",
        ],
    ),
    (
        "Faces",
        &[
            "😀", "😅", "😂", "🙂", "😉", "😍", "🤔", "😐", "😴", "😢", "😡", "🤯", "😎", "🥳",
            "🤝", "🙏",
        ],
    ),
    (
        "Signals",
        &[
            "👍", "👎", "👏", "🔥", "✅", "❌", "⚠️", "❤️", "⭐", "🎉", "🚀", "💡", "🔒", "📌",
            "⏳", "💬",
        ],
    ),
];

#[function_component(Picker)]
pub fn picker(p: &PickerProps) -> Html {
    let lang = crate::state::use_store().language;
    let tab = use_state(|| 0usize);

    let onkeydown = {
        let on_close = p.on_close.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Escape" {
                e.stop_propagation();
                on_close.emit(());
            }
        })
    };

    let (_, glyphs) = EMOTICONS[(*tab).min(EMOTICONS.len() - 1)];

    html! {
        <Popover
            open={p.open}
            class="fn-picker"
            role="dialog"
            label={t(lang, Key::choose_emoticon)}
            on_dismiss={p.on_close.clone()}
            {onkeydown}
        >
            <div class="fn-tabs" role="tablist" aria-label={t(lang, Key::emoticon_categories)}>
                { for EMOTICONS.iter().enumerate().map(|(i, (name, _))| {
                    let select = {
                        let tab = tab.clone();
                        Callback::from(move |_: MouseEvent| tab.set(i))
                    };
                    html! {
                        <button
                            type="button"
                            class="fn-tab"
                            role="tab"
                            aria-selected={(*tab == i).to_string()}
                            onclick={select}
                        >{ name }</button>
                    }
                }) }
            </div>
            { for glyphs.iter().enumerate().map(|(i, g)| {
                let pick = {
                    let on_pick = p.on_pick.clone();
                    let g = (*g).to_owned();
                    Callback::from(move |_: MouseEvent| on_pick.emit(g.clone()))
                };
                html! {
                    // `--i` staggers the grid entrance (app.css §14, 12ms apart).
                    <button
                        type="button"
                        class="fn-picker__cell"
                        style={format!("--i: {i}")}
                        aria-label={t(lang, Key::react_with).replace("{emoji}", g)}
                        onclick={pick}
                    >
                        { g }
                    </button>
                }
            }) }
        </Popover>
    }
}
