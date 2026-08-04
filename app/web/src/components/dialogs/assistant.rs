//! The AI assistant (ported from the reference client's `AIAssistant.tsx`).
//!
//! Five tabs: **Write** (free prompt → draft), **Reply** (suggest a reply
//! from recent room context), **Image** and **Video** (prompt → media this
//! server hosts), and **Keys** (per-provider API keys with a live
//! connectivity test).
//!
//! Privacy note, stated in the UI as well: the Reply tab decrypts recent
//! room messages *on this device* and sends them to the selected provider.
//! That is the user's explicit choice per use — nothing leaves the device
//! until a generate button is pressed. Keys live in localStorage only; the
//! PocketSkynet server never sees keys, prompts, or context.
//!
//! # Generated media is stored here, never linked from the provider
//!
//! Every generation ends up at `/api/images/<sha256>.<ext>` on this server
//! before it can be posted or copied. The provider's own link is temporary —
//! xAI's expires in about a day — and a room full of dead pictures is worse
//! than no pictures. So bytes are uploaded, and a URL-only answer (which is
//! all video generation offers) is handed to the server to fetch. The room
//! carries the same-origin path, which the message renderer draws as the
//! picture or the clip itself.
//!
//! # Waiting is shown, not implied
//!
//! Image generation takes seconds and video takes minutes, which is long
//! enough that a button reading "Generating…" looks like a hang. Every call
//! raises a live waiting row: what is being waited on, and how long it has
//! been. See [`waiting_row`].

use std::rc::Rc;

use pocketskynet_core::RoomId;
use web_sys::{HtmlInputElement, HtmlTextAreaElement};
use yew::prelude::*;

use crate::ai::{self, AiSettings, Provider, VideoStatus};
use crate::state::{use_store, Action};
use crate::{actions, crypto, format};

use super::super::common::Spinner;
use super::super::modal::Modal as Dialog;
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
    Video,
    Keys,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Write, Tab::Reply, Tab::Image, Tab::Video, Tab::Keys];
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Tab::Write => t(lang, Key::tab_write),
            Tab::Reply => t(lang, Key::tab_reply),
            Tab::Image => t(lang, Key::tab_image),
            Tab::Video => t(lang, Key::tab_video),
            Tab::Keys => t(lang, Key::tab_keys),
        }
    }
}

/// What the assistant is waiting on, for the waiting row's label.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Text,
    Image,
    Video,
    /// The provider has answered; the bytes are being stored here.
    Hosting,
}

impl Phase {
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Phase::Text => t(lang, Key::ai_writing),
            Phase::Image => t(lang, Key::ai_drawing),
            Phase::Video => t(lang, Key::ai_filming),
            Phase::Hosting => t(lang, Key::ai_saving_here),
        }
    }
}

/// One in-flight request: what it is, and when it started.
///
/// The start time rather than a counter, because the elapsed reading is
/// derived from a clock the whole dialog shares — a counter incremented from
/// inside a timer callback captures a stale state handle and freezes at one.
#[derive(Clone, Copy, PartialEq)]
struct Busy {
    phase: Phase,
    started: i64,
}

/// A finished generation, hosted here.
#[derive(Clone, PartialEq)]
struct Generated {
    /// The same-origin path, which is what gets posted to the room: relative,
    /// so it keeps resolving whether this server is reached over loopback,
    /// the LAN, or a mesh VPN.
    url: String,
    is_video: bool,
}

/// How often the video generation is asked whether it is done. The provider
/// bills nothing for a poll, but a tighter loop is still just noise on a job
/// measured in minutes.
const VIDEO_POLL_MS: u32 = 4_000;

/// When to stop asking. Ten minutes is past anything the provider documents
/// for a six-second clip, so reaching it means something is wrong rather than
/// slow — and an infinite loop in a dialog nobody can close is worse than a
/// clear failure.
const VIDEO_POLL_LIMIT: u32 = 150;

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
    let media = use_state(|| Option::<Generated>::None);
    let busy = use_state(|| Option::<Busy>::None);
    // The Keys tab's Test button runs its own request with its own flag: it
    // is a connectivity check, not a generation, and it has no waiting row.
    let key_busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);

    // The clock the waiting row reads. It ticks only while something is in
    // flight, so an idle dialog re-renders exactly never.
    let now = use_state(format::now_ms);
    {
        let now = now.clone();
        use_effect_with(busy.is_some(), move |&waiting| {
            let interval = waiting.then(|| {
                gloo_timers::callback::Interval::new(500, move || now.set(format::now_ms()))
            });
            move || drop(interval)
        });
    }

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
            if busy.is_some() {
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
            busy.set(Some(Busy {
                phase: Phase::Text,
                started: format::now_ms(),
            }));
            error.set(None);
            let draft = draft.clone();
            let busy = busy.clone();
            let error = error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match ai::generate_text(provider, &key, SYSTEM, &user).await {
                    Ok(text) => draft.set(text),
                    Err(e) => error.set(Some(e)),
                }
                busy.set(None);
            });
        })
    };

    // Image and video are one flow with two endpoints: generate, store the
    // result *here*, then show it. Nothing the provider hosts is ever handed
    // to the room, so the two paths cannot drift on the part that matters.
    let generate_media = {
        let store = store.clone();
        let settings = settings.clone();
        let prompt = prompt.clone();
        let media = media.clone();
        let busy = busy.clone();
        let error = error.clone();
        Rc::new(move |video: bool| {
            if busy.is_some() {
                return;
            }
            let chosen = if video {
                settings.video_provider()
            } else {
                settings.image_provider()
            };
            let Some(provider) = chosen else {
                error.set(Some(if video {
                    t(lang, Key::video_needs_grok).to_owned()
                } else {
                    "Add a Grok, OpenAI or Gemini key in the Keys tab first.".to_owned()
                }));
                return;
            };
            let key = settings.key_for(provider).unwrap_or_default().to_owned();
            let p = prompt.trim().to_owned();
            if p.is_empty() {
                error.set(Some(if video {
                    "Describe the video.".into()
                } else {
                    "Describe the image.".into()
                }));
                return;
            }
            // Held rather than re-read: a state handle keeps the value it was
            // rendered with, so the task below would otherwise see the `None`
            // this call is replacing.
            let started = format::now_ms();
            busy.set(Some(Busy {
                phase: if video { Phase::Video } else { Phase::Image },
                started,
            }));
            error.set(None);
            media.set(None);

            let client = store.client.clone();
            let media = media.clone();
            let busy = busy.clone();
            let error = error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let hosting = {
                    let busy = busy.clone();
                    move || {
                        busy.set(Some(Busy {
                            phase: Phase::Hosting,
                            started,
                        }))
                    }
                };
                let result = if video {
                    generate_video(&client, provider, &key, &p, lang, hosting).await
                } else {
                    generate_image(&client, provider, &key, &p, hosting).await
                };
                match result {
                    Ok(generated) => media.set(Some(generated)),
                    Err(e) => error.set(Some(e)),
                }
                busy.set(None);
            });
        })
    };
    let on_generate_image = {
        let generate_media = generate_media.clone();
        Callback::from(move |_: MouseEvent| generate_media(false))
    };
    let on_generate_video = {
        let generate_media = generate_media.clone();
        Callback::from(move |_: MouseEvent| generate_media(true))
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
                            disabled={busy.is_some() || provider.is_none()}
                            onclick={generate_text}
                        >
                            { if busy.is_some() { t(lang, Key::generating) } else { t(lang, Key::generate) } }
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
        Tab::Image | Tab::Video => {
            let video = *tab == Tab::Video;
            let provider = if video {
                settings.video_provider()
            } else {
                settings.image_provider()
            };
            let (placeholder, label) = if video {
                (
                    "Describe the video to generate…",
                    t(lang, Key::generate_video),
                )
            } else {
                (
                    "Describe the image to generate…",
                    t(lang, Key::generate_image),
                )
            };
            html! {
                <>
                    { prompt_input(placeholder) }
                    <div class="fn-row">
                        <button
                            type="button"
                            class="topcoat-button--cta"
                            disabled={busy.is_some() || provider.is_none()}
                            onclick={if video { on_generate_video.clone() } else { on_generate_image.clone() }}
                        >
                            { if busy.is_some() { t(lang, Key::generating) } else { label } }
                        </button>
                        if let Some(pr) = provider {
                            <span class="fn-field__help">{ format!("via {}", pr.label()) }</span>
                        } else if video {
                            <span class="fn-field__help">{ t(lang, Key::video_needs_grok) }</span>
                        }
                    </div>
                    // The result, its permanent link, and the post button.
                    // Cleared when a new generation starts, so what is shown
                    // is never the previous prompt's picture.
                    if let Some(generated) = &*media {
                        { media_panel(lang, &store, generated, &post) }
                    }
                </>
            }
        }
        Tab::Keys => keys_tab(lang, &settings, &key_busy, &error),
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
                // Below the body rather than inside each tab: one row, in one
                // place, whatever is being waited on.
                if let Some(b) = &*busy {
                    { waiting_row(lang, b, *now) }
                }
                if let Some(e) = &*error {
                    <p class="fn-field__error" role="alert">{ e }</p>
                }
            </div>
        </Dialog>
    }
}

/// Generate a still and store it here. `hosting` is called once the provider
/// has answered, so the waiting row can stop saying "generating" while the
/// bytes are being saved.
async fn generate_image(
    client: &crate::api::Client,
    provider: Provider,
    key: &str,
    prompt: &str,
    hosting: impl FnOnce(),
) -> Result<Generated, String> {
    let out = ai::generate_image(provider, key, prompt).await?;
    hosting();
    let url = ai::host_generation(client, out).await?;
    Ok(Generated {
        url,
        is_video: false,
    })
}

/// Generate a clip and store it here.
///
/// Video is asynchronous at the provider: the first call only queues the job,
/// and the URL it eventually yields is temporary. Both facts are handled here
/// — poll until it is rendered, then hand the link to our own server to
/// fetch, so what the caller gets back is a path this server will still serve
/// long after the provider's link is gone.
async fn generate_video(
    client: &crate::api::Client,
    provider: Provider,
    key: &str,
    prompt: &str,
    lang: Lang,
    hosting: impl FnOnce(),
) -> Result<Generated, String> {
    let request_id = ai::start_video(provider, key, prompt).await?;
    let mut polls = 0;
    let provider_url = loop {
        sleep(VIDEO_POLL_MS).await;
        match ai::poll_video(provider, key, &request_id).await? {
            VideoStatus::Ready(url) => break url,
            VideoStatus::Pending => {
                polls += 1;
                if polls >= VIDEO_POLL_LIMIT {
                    return Err(t(lang, Key::ai_video_timeout).to_owned());
                }
            }
        }
    };
    hosting();
    let url = client
        .import_media(&provider_url)
        .await
        .map_err(|e| e.user_message())?;
    Ok(Generated {
        url,
        is_video: true,
    })
}

#[cfg(target_arch = "wasm32")]
async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep(_ms: u32) {}

/// The waiting row: what is being waited on, and for how long.
///
/// The elapsed reading is the point. A generation that takes four seconds and
/// one that has been stuck for four minutes look identical behind a spinner,
/// and the second is the one a person needs to know about — especially for
/// video, where minutes are normal and there is nothing else on screen to say
/// so.
fn waiting_row(lang: Lang, busy: &Busy, now: i64) -> Html {
    let seconds = ((now - busy.started).max(0) / 1_000) as u64;
    html! {
        <div class="fn-ai__wait" role="status" aria-live="polite">
            <Spinner />
            <span class="fn-ai__wait-text">{ busy.phase.label(lang) }</span>
            <span class="fn-ai__wait-time fn-nums">{ format::elapsed_clock(seconds) }</span>
        </div>
    }
}

/// The generated media, its permanent link, and what to do with either.
fn media_panel(
    lang: Lang,
    store: &crate::state::Store,
    generated: &Generated,
    post: &Callback<String>,
) -> Html {
    // Absolute for the clipboard, relative for the room: a link somebody
    // pastes into another app has to name the host, and a link posted into a
    // room must not, so it survives this server being reached at a different
    // address tomorrow.
    //
    // The host named is the server's own recommendation — a Tailscale address
    // where there is one — not this page's origin, which is usually
    // `127.0.0.1` and would point the recipient at their own machine.
    let absolute = store.shareable_url(&generated.url);
    let copy = {
        let store = store.clone();
        let absolute = absolute.clone();
        Callback::from(move |_: MouseEvent| {
            super::super::common::copy_with_toast(
                &store,
                &absolute,
                t(lang, Key::publish_url_copied),
            );
        })
    };
    html! {
        <>
            if generated.is_video {
                <video
                    class="fn-ai__preview"
                    src={generated.url.clone()}
                    controls=true
                    playsinline=true
                    preload="metadata"
                    aria-label={t(lang, Key::video_alt)}
                />
            } else {
                <img class="fn-ai__preview" src={generated.url.clone()} alt={t(lang, Key::image_alt)} />
            }
            <div class="fn-field">
                <label class="fn-field__label">{ t(lang, Key::media_link) }</label>
                <div class="fn-row">
                    <input
                        class="topcoat-text-input fn-grow fn-nums"
                        type="text"
                        readonly=true
                        value={absolute.clone()}
                        // Selecting the whole thing on focus is what makes
                        // this usable on a phone, where dragging a caret
                        // across a 100-character URL is the hard part.
                        onfocus={Callback::from(|e: FocusEvent| {
                            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                                el.select();
                            }
                        })}
                    />
                    <button type="button" class="topcoat-button" onclick={copy}>
                        { t(lang, Key::copy) }
                    </button>
                </div>
                <p class="fn-field__help">{ t(lang, Key::media_saved_here) }</p>
            </div>
            <div class="fn-row">
                <button
                    type="button"
                    class="topcoat-button--cta"
                    onclick={{
                        let post = post.clone();
                        let url = generated.url.clone();
                        Callback::from(move |_: MouseEvent| post.emit(url.clone()))
                    }}
                >
                    { t(lang, Key::post_to_room) }
                </button>
            </div>
        </>
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
