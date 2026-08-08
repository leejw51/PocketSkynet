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

use pocketskynet_core::WalletAddress;
use web_sys::HtmlTextAreaElement;
use yew::prelude::*;

use crate::state::PostBlock;

use super::common::{Ident, IdentSize, Popover};
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
    /// One or more files were picked: the browser's handles to them, and
    /// whatever was in the field at the time, which becomes their shared
    /// caption.
    ///
    /// The **handles**, deliberately — this used to hand up a `Vec<u8>` so
    /// that nothing outside the composer touched `web_sys::File`. That
    /// encapsulation cost the whole file in memory, twice, which a 4 GB cap
    /// cannot afford; the upload layer now reads each one in slices and never
    /// holds more than one at a time.
    #[prop_or_default]
    pub on_attach: Callback<(Vec<web_sys::File>, String)>,
    /// Opens the Files drawer for this room.
    #[prop_or_default]
    pub on_open_files: Callback<()>,
    /// The room's roster, for the `@` autocomplete. Empty disables it.
    #[prop_or_default]
    pub members: Vec<crate::api::RoomMember>,
    /// The viewer, so the autocomplete does not offer them themselves.
    #[prop_or_default]
    pub me: Option<WalletAddress>,
    /// Set when this message will be posted into a thread. Shown as a chip
    /// above the field, because a reply that silently went somewhere other
    /// than the channel is the worst outcome available here.
    #[prop_or_default]
    pub replying_to: Option<String>,
    #[prop_or_default]
    pub on_cancel_reply: Callback<MouseEvent>,
}

#[function_component(Composer)]
pub fn composer(p: &ComposerProps) -> Html {
    let lang = crate::state::use_store().language;
    let text = use_state(String::new);
    let area = use_node_ref();
    let send_btn = use_node_ref();
    let file_input = use_node_ref();
    let last_typing = use_mut_ref(|| 0f64);
    // Korean and every other composed script: Enter finishes the syllable
    // before it means "send". See `common::ImeGuard`.
    let ime = super::common::use_ime_guard(area.clone());
    // The `@` being completed, and which suggestion is highlighted.
    let mention = use_state(|| Option::<crate::mentions::ActiveMention>::None);
    let mention_index = use_state(|| 0usize);

    let suggestions: Vec<crate::mentions::Candidate> = match (&*mention, &p.me) {
        (Some(active), Some(me)) => crate::mentions::suggest(&p.members, me, &active.query),
        _ => Vec::new(),
    };
    // Bounded here rather than by scrolling: a list taller than the composer
    // covers the conversation it is meant to be about.
    let suggestions: Vec<_> = suggestions.into_iter().take(6).collect();
    let picked = mention_index.min(suggestions.len().saturating_sub(1));

    // Accept a suggestion: rewrite the field and put the caret after it.
    let accept = {
        let text = text.clone();
        let mention = mention.clone();
        let area = area.clone();
        std::rc::Rc::new(move |name: String| {
            let Some(active) = (*mention).clone() else {
                return;
            };
            let (next, caret) = crate::mentions::apply(&text, &active, &name);
            text.set(next.clone());
            mention.set(None);
            // The caret has to be restored explicitly: setting `value` puts it
            // at the end, which after a mid-sentence mention is the wrong end.
            if let Some(el) = area.cast::<HtmlTextAreaElement>() {
                el.set_value(&next);
                // Back into UTF-16 units for the DOM, or a caret after CJK
                // text lands past where it should.
                let units = crate::mentions::byte_to_caret(&next, caret) as u32;
                let _ = el.set_selection_range(units, units);
                let _ = el.focus();
            }
        })
    };

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
        let mention = mention.clone();
        let mention_index = mention_index.clone();
        Callback::from(move |e: InputEvent| {
            let Some(el) = e.target_dyn_into::<HtmlTextAreaElement>() else {
                return;
            };
            let value = el.value();
            // Where the caret is decides whether an `@` is being completed;
            // the text alone cannot say, because a finished mention earlier in
            // the line looks identical to one being typed now. The DOM reports
            // the caret in UTF-16 units; the scanner works in bytes — the
            // conversion is what keeps this working after Korean text, where
            // the two disagree.
            let units = el.selection_start().ok().flatten().unwrap_or(0) as usize;
            let caret = crate::mentions::caret_to_byte(&value, units);
            mention.set(crate::mentions::active_mention(&value, caret));
            mention_index.set(0);
            text.set(value);

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
        let mention = mention.clone();
        let mention_index = mention_index.clone();
        let accept = accept.clone();
        let names: Vec<String> = suggestions.iter().map(|c| c.name.clone()).collect();
        let ime = ime.clone();
        Callback::from(move |e: KeyboardEvent| {
            // Enter and Tab are the committing keys, and while an IME is
            // assembling a syllable they belong to it rather than to us — see
            // `common::ImeGuard`. Only those two: the arrows and Escape stay
            // live, or finishing a Hangul name would briefly freeze the
            // suggestion list's navigation.
            if matches!(e.key().as_str(), "Enter" | "Tab") && ime.blocks(&e) {
                return;
            }
            // The suggestion list owns the arrows, Enter, Tab and Escape while
            // it is open. Enter especially: with a list on screen it means
            // "this one", and sending the half-typed handle instead is the
            // mistake this whole branch exists to prevent.
            if !names.is_empty() {
                match e.key().as_str() {
                    "ArrowDown" => {
                        e.prevent_default();
                        mention_index.set((*mention_index + 1) % names.len());
                        return;
                    }
                    "ArrowUp" => {
                        e.prevent_default();
                        mention_index.set(mention_index.checked_sub(1).unwrap_or(names.len() - 1));
                        return;
                    }
                    "Enter" | "Tab" => {
                        e.prevent_default();
                        let i = (*mention_index).min(names.len() - 1);
                        accept(names[i].clone());
                        return;
                    }
                    "Escape" => {
                        e.stop_propagation();
                        mention.set(None);
                        return;
                    }
                    _ => {}
                }
            }
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

    // The pick happens here; the bytes are read by the upload layer, a slice at
    // a time. See `on_attach` for why the handle travels rather than a buffer.
    let onpick = {
        let on_attach = p.on_attach.clone();
        let file_input = file_input.clone();
        let text = text.clone();
        Callback::from(move |_: Event| {
            let Some(input) = file_input.cast::<web_sys::HtmlInputElement>() else {
                return;
            };
            let Some(list) = input.files() else {
                return;
            };
            // However many were picked — one or, with `multiple` on the input,
            // up to a browser-chosen batch; `attach_files` enforces the actual
            // per-message cap and tells the user if it trimmed the selection.
            let files: Vec<web_sys::File> =
                (0..list.length()).filter_map(|i| list.get(i)).collect();
            if files.is_empty() {
                return;
            }
            // Clear the input *now*, so picking the same file(s) twice in a row
            // still fires `change` the second time.
            input.set_value("");

            // Whatever is in the field becomes the caption, shared by every
            // file in this pick, and the field clears. This is the only way to
            // tag an attachment, and making it the composer rather than a
            // second dialog is deliberate: type "#q3 #finance", attach, done. A
            // modal asking for tags after every pick would be answered with an
            // empty box nine times in ten.
            let caption = text.trim().to_owned();
            text.set(String::new());

            // The handles go up, not the bytes. `array_buffer()` on the whole
            // file used to happen here, which put the entire attachment in the
            // wasm heap — and then `Uint8Array::from` put a second copy beside
            // it. At 25 MB that was merely wasteful; the cap is 4 GB now, which
            // is the *entire* wasm32 address space, so reading a file here
            // would be the one line that makes a large upload impossible.
            // `crate::api::uploads` reads each one a slice at a time instead.
            on_attach.emit((files, caption));
        })
    };

    let locked = p.blocked.is_some();
    let placeholder = match p.blocked {
        Some(reason) => reason.composer_hint().to_owned(),
        None => t(lang, Key::message_placeholder).replace("{room}", &p.room_name),
    };

    html! {
        <div class="fn-composer" data-locked={locked.to_string()}>
            if let Some(who) = &p.replying_to {
                <div class="fn-composer__reply">
                    { icons::thread(14) }
                    <span>{ t(lang, Key::reply_in_thread) }{ " · " }{ who }</span>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::cancel)}
                        onclick={p.on_cancel_reply.clone()}
                    >{ icons::close(14) }</button>
                </div>
            }
            if !suggestions.is_empty() {
                <ul class="fn-mention-pop" role="listbox"
                    aria-label={t(lang, Key::mention_suggestions)}>
                    { for suggestions.iter().enumerate().map(|(i, c)| {
                        let accept = accept.clone();
                        let name = c.name.clone();
                        html! {
                            <li
                                key={c.address.to_string()}
                                role="option"
                                aria-selected={(i == picked).to_string()}
                                class={classes!((i == picked).then_some("is-active"))}
                                // `mousedown`, not `click`: a click fires after
                                // the textarea has already lost focus, and the
                                // blur closes this list out from under it.
                                onmousedown={Callback::from(move |e: MouseEvent| {
                                    e.prevent_default();
                                    accept(name.clone());
                                })}
                            >
                                <Ident seed={c.address.to_string()} size={IdentSize::Xs}
                                       image={c.image.clone()} />
                                <span>{ &c.name }</span>
                            </li>
                        }
                    }) }
                </ul>
            }
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
                multiple=true
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
