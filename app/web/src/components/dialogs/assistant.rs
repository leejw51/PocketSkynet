//! The AI assistant (ported from the reference client's `AIAssistant.tsx`).
//!
//! Four tabs: **Write** (free prompt → draft), **Reply** (suggest a reply
//! from recent room context), **Image** (prompt → hosted image URL), and
//! **Keys** (per-provider API keys with a live connectivity test).
//!
//! Privacy note, stated in the UI as well: the Reply tab decrypts recent
//! room messages *on this device* and sends them to the selected provider.
//! That is the user's explicit choice per use — nothing leaves the device
//! until a generate button is pressed. Keys live in localStorage only; the
//! PocketSkynet server never sees keys, prompts, or context.

use pocketskynet_core::RoomId;
use web_sys::{HtmlInputElement, HtmlTextAreaElement};
use yew::prelude::*;

use crate::ai::{self, AiSettings, ImageOut, Provider};
use crate::state::{use_store, Action};
use crate::{actions, crypto, format};

use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key, Lang};

#[derive(Properties, PartialEq)]
pub struct AssistantProps {
    pub room_id: RoomId,
    pub on_close: Callback<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Write,
    Reply,
    Image,
    Keys,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Write, Tab::Reply, Tab::Image, Tab::Keys];
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Tab::Write => t(lang, Key::tab_write),
            Tab::Reply => t(lang, Key::tab_reply),
            Tab::Image => t(lang, Key::tab_image),
            Tab::Keys => t(lang, Key::tab_keys),
        }
    }
}

/// The system prompt shared by the text tabs: a chat participant, not an
/// essayist — the output is pasted into a room.
const SYSTEM: &str = "You are a helpful assistant drafting a chat message on the user's behalf. \
     Answer with the message text only — no preamble, no quotes, no markdown fences.";

#[function_component(Assistant)]
pub fn assistant(p: &AssistantProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let settings = use_state(AiSettings::load);
    let tab = use_state(|| {
        if AiSettings::load().any_key() {
            Tab::Write
        } else {
            Tab::Keys
        }
    });
    let prompt = use_state(String::new);
    let draft = use_state(String::new);
    let image_url = use_state(|| Option::<String>::None);
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);

    // ---- helpers ---------------------------------------------------------

    // Post text into the room through the normal optimistic-send path, so
    // AI output is encrypted exactly like something the user typed.
    let post = {
        let store = store.clone();
        let room_id = p.room_id.clone();
        let on_close = p.on_close.clone();
        Callback::from(move |text: String| {
            let text = text.trim().to_owned();
            if text.is_empty() {
                return;
            }
            let now = format::now_ms();
            let local_id = crate::state::next_local_id();
            store.dispatch(Action::QueueSend(
                room_id.clone(),
                local_id,
                text.clone(),
                now,
            ));
            let store2 = store.clone();
            let room_id2 = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                actions::send_message(store2, room_id2, local_id, text).await;
            });
            on_close.emit(());
        })
    };

    // The last few messages as "name: text" lines, decrypted locally.
    let context = {
        let store = store.clone();
        let room_id = p.room_id.clone();
        move || -> String {
            let Some(room_state) = store.room_states.get(&room_id) else {
                return String::new();
            };
            let bundle = store.bundle(&room_id);
            room_state
                .ordered(&store.blocks)
                .iter()
                .rev()
                .take(10)
                .rev()
                .filter_map(|msg| {
                    let who = msg
                        .sender
                        .as_ref()
                        .map(|u| u.username.clone())
                        .unwrap_or_else(|| msg.sender_address.abbreviated());
                    let text = if msg.is_encrypted {
                        let bundle = bundle.as_ref()?;
                        crypto::decrypt_message(bundle, &room_id, msg)
                            .text()
                            .map(str::to_owned)?
                    } else {
                        msg.content.clone()
                    };
                    Some(format!("{who}: {text}"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    };

    let generate_text = {
        let settings = settings.clone();
        let prompt = prompt.clone();
        let draft = draft.clone();
        let busy = busy.clone();
        let error = error.clone();
        let tab = tab.clone();
        let context = context.clone();
        Callback::from(move |_: MouseEvent| {
            if *busy {
                return;
            }
            let Some(provider) = settings.text_provider() else {
                error.set(Some("Add an API key in the Keys tab first.".into()));
                return;
            };
            let key = settings.key_for(provider).unwrap_or_default().to_owned();
            let user = match *tab {
                Tab::Reply => {
                    let ctx = context();
                    if ctx.is_empty() {
                        error.set(Some("No readable messages to reply to yet.".into()));
                        return;
                    }
                    format!(
                        "Recent conversation:\n{ctx}\n\nSuggest a natural reply from me.{}",
                        if prompt.trim().is_empty() {
                            String::new()
                        } else {
                            format!(" Guidance: {}", prompt.trim())
                        }
                    )
                }
                _ => {
                    if prompt.trim().is_empty() {
                        error.set(Some("Describe what to write.".into()));
                        return;
                    }
                    prompt.trim().to_owned()
                }
            };
            busy.set(true);
            error.set(None);
            let draft = draft.clone();
            let busy = busy.clone();
            let error = error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match ai::generate_text(provider, &key, SYSTEM, &user).await {
                    Ok(text) => draft.set(text),
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        })
    };

    let generate_image = {
        let store = store.clone();
        let settings = settings.clone();
        let prompt = prompt.clone();
        let image_url = image_url.clone();
        let busy = busy.clone();
        let error = error.clone();
        Callback::from(move |_: MouseEvent| {
            if *busy {
                return;
            }
            let Some(provider) = settings.image_provider() else {
                error.set(Some(
                    "Add a Grok, OpenAI or Gemini key in the Keys tab first.".into(),
                ));
                return;
            };
            let key = settings.key_for(provider).unwrap_or_default().to_owned();
            let p = prompt.trim().to_owned();
            if p.is_empty() {
                error.set(Some("Describe the image.".into()));
                return;
            }
            busy.set(true);
            error.set(None);
            let store = store.clone();
            let image_url = image_url.clone();
            let busy = busy.clone();
            let error = error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let out = ai::generate_image(provider, &key, &p).await;
                match out {
                    Ok(ImageOut::Url(url)) => image_url.set(Some(url)),
                    Ok(ImageOut::Bytes { mime, bytes }) => {
                        // Host the bytes on our own server so the room gets a
                        // stable same-origin URL instead of a megabyte of
                        // base64.
                        match store.client.upload_image(&mime, bytes).await {
                            Ok(url) => image_url.set(Some(url)),
                            Err(e) => error.set(Some(
                                t(lang, Key::hosting_failed).replace("{error}", &e.to_string()),
                            )),
                        }
                    }
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        })
    };

    // ---- render ----------------------------------------------------------

    let close = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: ()| on_close.emit(()))
    };

    let tabs = html! {
        <div class="fn-ai__tabs" role="tablist" aria-label={t(lang, Key::assistant)}>
            { for Tab::ALL.iter().map(|&t| {
                let tab = tab.clone();
                let error = error.clone();
                html! {
                    <button
                        type="button"
                        class="fn-ai__tab"
                        role="tab"
                        aria-selected={(t == *tab).then_some("true")}
                        onclick={Callback::from(move |_: MouseEvent| {
                            tab.set(t);
                            error.set(None);
                        })}
                    >
                        { t.label(lang) }
                    </button>
                }
            }) }
        </div>
    };

    let prompt_input = |placeholder: &'static str| {
        let prompt = prompt.clone();
        html! {
            <textarea
                class="topcoat-textarea"
                rows="3"
                {placeholder}
                value={(*prompt).clone()}
                oninput={Callback::from(move |e: InputEvent| {
                    if let Some(el) = e.target_dyn_into::<HtmlTextAreaElement>() {
                        prompt.set(el.value());
                    }
                })}
            />
        }
    };

    let body = match *tab {
        Tab::Write | Tab::Reply => {
            let placeholder = if *tab == Tab::Write {
                "What should the message say?"
            } else {
                "Optional guidance for the reply (tone, content)…"
            };
            let provider = settings.text_provider();
            html! {
                <>
                    if *tab == Tab::Reply {
                        <p class="fn-field__help">
                            { t(lang, Key::reply_suggestion_hint) }
                        </p>
                    }
                    { prompt_input(placeholder) }
                    <div class="fn-row">
                        <button
                            type="button"
                            class="topcoat-button--cta"
                            disabled={*busy || provider.is_none()}
                            onclick={generate_text}
                        >
                            { if *busy { t(lang, Key::generating) } else { t(lang, Key::generate) } }
                        </button>
                        if let Some(pr) = provider {
                            <span class="fn-field__help">{ format!("via {}", pr.label()) }</span>
                        }
                    </div>
                    if !draft.is_empty() {
                        <textarea
                            class="topcoat-textarea fn-ai__draft"
                            rows="5"
                            value={(*draft).clone()}
                            oninput={{
                                let draft = draft.clone();
                                Callback::from(move |e: InputEvent| {
                                    if let Some(el) = e.target_dyn_into::<HtmlTextAreaElement>() {
                                        draft.set(el.value());
                                    }
                                })
                            }}
                        />
                        <div class="fn-row">
                            <button
                                type="button"
                                class="topcoat-button--cta"
                                onclick={{
                                    let post = post.clone();
                                    let draft = draft.clone();
                                    Callback::from(move |_: MouseEvent| post.emit((*draft).clone()))
                                }}
                            >
                                { t(lang, Key::post_to_room) }
                            </button>
                            <button
                                type="button"
                                class="topcoat-button"
                                onclick={{
                                    let store = store.clone();
                                    let draft = draft.clone();
                                    Callback::from(move |_: MouseEvent| {
                                        super::super::common::copy_with_toast(
                                            &store,
                                            &draft,
                                            t(lang, Key::copied),
                                        );
                                    })
                                }}
                            >
                                { t(lang, Key::copy) }
                            </button>
                        </div>
                    }
                </>
            }
        }
        Tab::Image => html! {
            <>
                { prompt_input("Describe the image to generate…") }
                <div class="fn-row">
                    <button
                        type="button"
                        class="topcoat-button--cta"
                        disabled={*busy || settings.image_provider().is_none()}
                        onclick={generate_image}
                    >
                        { if *busy { t(lang, Key::generating) } else { t(lang, Key::generate_image) } }
                    </button>
                    if let Some(pr) = settings.image_provider() {
                        <span class="fn-field__help">{ format!("via {}", pr.label()) }</span>
                    }
                </div>
                if let Some(url) = &*image_url {
                    <img class="fn-ai__preview" src={url.clone()} alt="Generated image" />
                    <div class="fn-row">
                        <button
                            type="button"
                            class="topcoat-button--cta"
                            onclick={{
                                let post = post.clone();
                                let url = url.clone();
                                Callback::from(move |_: MouseEvent| post.emit(url.clone()))
                            }}
                        >
                            { t(lang, Key::post_to_room) }
                        </button>
                    </div>
                }
            </>
        },
        Tab::Keys => keys_tab(lang, &settings, &busy, &error),
    };

    html! {
        <Dialog
            title={t(lang, Key::ai_assistant)}
            description={t(lang, Key::keys_stay_in_browser)}
            wide=true
            busy={false}
            on_close={close}
            footer={None::<Html>}
        >
            <div class="fn-ai">
                { tabs }
                { body }
                if let Some(e) = &*error {
                    <p class="fn-field__error" role="alert">{ e }</p>
                }
            </div>
        </Dialog>
    }
}

/// Device-wide AI key management, embeddable outside the assistant — the
/// Settings page mounts this, because "where do I put my API key" must not
/// require first opening a chat room and finding the composer's ✨ button.
/// Same rows, same storage (`ps-ai`), same live Test button.
#[function_component(AiKeysEditor)]
pub fn ai_keys_editor() -> Html {
    let lang = crate::state::use_store().language;
    let settings = use_state(AiSettings::load);
    let busy = use_state(|| false);
    let status = use_state(|| Option::<String>::None);
    html! {
        <>
            { keys_tab(lang, &settings, &busy, &status) }
            if let Some(s) = &*status {
                // Test results land here: "OK — replied …" as much as errors.
                <p class="fn-field__help" role="status">{ s }</p>
            }
        </>
    }
}

/// The Keys tab: one row per provider — key input, hint, and a Test button
/// that fires a real one-word request and reports what came back.
fn keys_tab(
    lang: Lang,
    settings: &UseStateHandle<AiSettings>,
    busy: &UseStateHandle<bool>,
    error: &UseStateHandle<Option<String>>,
) -> Html {
    html! {
        <div class="fn-ai__keys">
            { for Provider::ALL.iter().map(|&provider| {
                let current = settings.key_for(provider).unwrap_or_default().to_owned();
                let on_key = {
                    let settings = settings.clone();
                    Callback::from(move |e: InputEvent| {
                        if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                            let mut next = (*settings).clone();
                            next.set_key(provider, &el.value());
                            next.save();
                            settings.set(next);
                        }
                    })
                };
                let test = {
                    let settings = settings.clone();
                    let busy = busy.clone();
                    let error = error.clone();
                    Callback::from(move |_: MouseEvent| {
                        let Some(key) = settings.key_for(provider).map(str::to_owned) else {
                            return;
                        };
                        busy.set(true);
                        error.set(None);
                        let error = error.clone();
                        let busy = busy.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            match ai::test_key(provider, &key).await {
                                Ok(reply) => error.set(Some(format!(
                                    "{}: OK — replied {reply:?}",
                                    provider.label()
                                ))),
                                Err(e) => {
                                    error.set(Some(format!("{}: {e}", provider.label())))
                                }
                            }
                            busy.set(false);
                        });
                    })
                };
                html! {
                    <div class="fn-field">
                        <label class="fn-field__label">{ provider.label() }</label>
                        <div class="fn-row">
                            <input
                                class="topcoat-text-input fn-grow fn-nums"
                                type="password"
                                placeholder={provider.key_hint()}
                                autocomplete="off"
                                value={current.clone()}
                                oninput={on_key}
                            />
                            <button
                                type="button"
                                class="topcoat-button"
                                disabled={current.is_empty() || **busy}
                                onclick={test}
                            >
                                { t(lang, Key::test) }
                            </button>
                        </div>
                    </div>
                }
            }) }
            <p class="fn-field__help">
                { t(lang, Key::anthropic_text_only) }
            </p>
        </div>
    }
}
