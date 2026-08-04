//! The Knowledge page (docs/SEARCH.md §5) — the product's second verb.
//!
//! Chat rooms remember themselves: everything written in a plaintext room is
//! already retrievable here. This page adds the two verbs on top:
//!
//! * **Search** — one box over everything visible: messages, taught notes,
//!   `#tags`. Retrieval is entirely server-side SQLite (BM25 ⊕ local
//!   embeddings); nothing leaves the LAN.
//! * **Teach** — write a note the server keeps and everyone on it can find.
//!
//! # Cloud AI, and why it asks first
//!
//! When this device's assistant settings hold a text-provider key, a search
//! can be escalated to an AI answer built *from the retrieved results* (the
//! generation half of RAG). That crosses a line the rest of the page never
//! crosses — content leaves the self-hosted server — so it happens only on an
//! explicit button naming the provider, behind a consent card that says
//! exactly what will be sent. No key, no button; the page is fully useful
//! without one.

use yew::prelude::*;

use crate::api::search::{KnowledgeNote, SearchHit, TagCount};
use crate::format;
use crate::i18n::{t, Key};
use crate::route::Route;
use crate::state::{use_store, Action, KnowledgeSeed};
use crate::{ai, state};

use super::common::{Addr, Back, Badge, BusyButton, Empty};
use super::icons;
use super::toast;

use pocketskynet_core::{RoomId, WalletAddress};

#[derive(Properties, PartialEq)]
pub struct KnowledgeProps {
    pub on_navigate: Callback<Route>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Search,
    Teach,
}

/// One in-flight or settled AI escalation.
#[derive(Clone, PartialEq)]
enum Ask {
    /// The consent card is showing; nothing has been sent.
    Consent,
    Busy,
    Answered(String),
    Failed(String),
}

const RESULT_LIMIT: usize = 30;
/// How many retrieved passages an AI escalation may carry. Enough context to
/// answer from, few enough that the consent card's claim stays reviewable.
const ASK_PASSAGES: usize = 8;

#[function_component(Knowledge)]
pub fn knowledge(p: &KnowledgeProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let now = format::now_ms();

    let mode = use_state(|| Mode::Search);
    let query = use_state(String::new);
    let results = use_state(Vec::<SearchHit>::new);
    let searched = use_state(|| false);
    let busy = use_state(|| false);
    let tags = use_state(Vec::<TagCount>::new);
    let notes = use_state(Vec::<KnowledgeNote>::new);
    let draft = use_state(String::new);
    let note_filter = use_state(String::new);
    let refreshing = use_state(|| false);
    let teach_source = use_state(|| Option::<(Option<RoomId>, Option<String>)>::None);
    let teaching = use_state(|| false);
    let ask = use_state(|| Option::<Ask>::None);
    // Armed by the quick bar's "AI SEARCH": fire the AI answer as soon as
    // the retrieval lands, no consent card — the bar's label was the consent.
    let auto_ask = use_state(|| false);
    // Search generation counter: a slow response from an old query must not
    // clobber the results of a newer one.
    let generation = use_mut_ref(|| 0u64);

    let run_search = {
        let store = store.clone();
        let results = results.clone();
        let searched = searched.clone();
        let busy = busy.clone();
        let ask = ask.clone();
        let generation = generation.clone();
        Callback::from(move |q: String| {
            let store = store.clone();
            let results = results.clone();
            let searched = searched.clone();
            let busy = busy.clone();
            let ask = ask.clone();
            *generation.borrow_mut() += 1;
            let my_generation = *generation.borrow();
            let generation = generation.clone();
            busy.set(true);
            ask.set(None);
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = store.client.search(&q, RESULT_LIMIT).await;
                if *generation.borrow() != my_generation {
                    return; // a newer search superseded this one
                }
                busy.set(false);
                searched.set(true);
                match outcome {
                    Ok(hits) => results.set(hits),
                    Err(e) => {
                        results.set(Vec::new());
                        toast::error(&store, "Search failed", Some(e.user_message()));
                    }
                }
            });
        })
    };

    // Mount: take the seed (hashtag chip / "teach from message"), load the
    // tag cloud and the taught notes.
    {
        let store = store.clone();
        let mode = mode.clone();
        let query = query.clone();
        let draft = draft.clone();
        let teach_source = teach_source.clone();
        let run_search = run_search.clone();
        let tags = tags.clone();
        let notes = notes.clone();
        let auto_ask = auto_ask.clone();
        use_effect_with((), move |_| {
            match store.knowledge_seed.clone() {
                Some(KnowledgeSeed::Search { query: q, ask }) => {
                    store.dispatch(Action::TakeKnowledgeSeed);
                    query.set(q.clone());
                    auto_ask.set(ask);
                    run_search.emit(q);
                }
                Some(KnowledgeSeed::Teach {
                    content,
                    room_id,
                    message_id,
                }) => {
                    store.dispatch(Action::TakeKnowledgeSeed);
                    draft.set(content);
                    teach_source.set(Some((room_id, message_id.map(|m| m.to_string()))));
                    mode.set(Mode::Teach);
                }
                None => {}
            }
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(list) = store.client.search_tags(24).await {
                    tags.set(list);
                }
                if let Ok(list) = store.client.knowledge(100).await {
                    notes.set(list);
                }
            });
            || ()
        });
    }

    let me: Option<WalletAddress> = store.me().cloned();

    let on_query_input = {
        let query = query.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                query.set(el.value());
            }
        })
    };
    let on_query_key = {
        let query = query.clone();
        let run_search = run_search.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                run_search.emit((*query).clone());
            }
        })
    };

    let pick_tag = {
        let query = query.clone();
        let run_search = run_search.clone();
        let mode = mode.clone();
        Callback::from(move |tag: String| {
            let q = format!("#{tag}");
            mode.set(Mode::Search);
            query.set(q.clone());
            run_search.emit(q);
        })
    };

    // Pull the notes and the tag cloud fresh from the server — someone else
    // may have taught (or forgotten) since this page mounted.
    let reload_notes = {
        let store = store.clone();
        let notes = notes.clone();
        let tags = tags.clone();
        let refreshing = refreshing.clone();
        Callback::from(move |_: MouseEvent| {
            if *refreshing {
                return;
            }
            refreshing.set(true);
            let store = store.clone();
            let notes = notes.clone();
            let tags = tags.clone();
            let refreshing = refreshing.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = store.client.knowledge(100).await;
                refreshing.set(false);
                match outcome {
                    Ok(list) => notes.set(list),
                    Err(e) => {
                        toast::error(
                            &store,
                            t(store.language, Key::couldnt_load_notes),
                            Some(e.user_message()),
                        );
                        return;
                    }
                }
                if let Ok(list) = store.client.search_tags(24).await {
                    tags.set(list);
                }
            });
        })
    };

    let teach_now = {
        let store = store.clone();
        let draft = draft.clone();
        let teaching = teaching.clone();
        let notes = notes.clone();
        let teach_source = teach_source.clone();
        let tags = tags.clone();
        Callback::from(move |_: MouseEvent| {
            let content = draft.trim().to_owned();
            if content.is_empty() || *teaching {
                return;
            }
            teaching.set(true);
            let store = store.clone();
            let draft = draft.clone();
            let teaching = teaching.clone();
            let notes = notes.clone();
            let tags = tags.clone();
            let source = (*teach_source).clone();
            let teach_source = teach_source.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let (room_id, message_id) = source.unwrap_or((None, None));
                let outcome = store
                    .client
                    .teach(
                        &content,
                        room_id.as_ref().map(|r| r.as_str()),
                        message_id.as_deref(),
                    )
                    .await;
                teaching.set(false);
                match outcome {
                    Ok(note) => {
                        crate::progression::award(
                            pocketskynet_core::progression::Award::KnowledgeStored,
                        );
                        toast::success(&store, t(store.language, Key::taught_ok));
                        draft.set(String::new());
                        teach_source.set(None);
                        let mut list = (*notes).clone();
                        list.insert(0, note);
                        notes.set(list);
                        if let Ok(list) = store.client.search_tags(24).await {
                            tags.set(list);
                        }
                    }
                    Err(e) => toast::error(
                        &store,
                        t(store.language, Key::teach_failed),
                        Some(e.user_message()),
                    ),
                }
            });
        })
    };

    let forget = {
        let store = store.clone();
        let notes = notes.clone();
        let results = results.clone();
        Callback::from(move |id: String| {
            let store = store.clone();
            let notes = notes.clone();
            let results = results.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.forget(&id).await {
                    Ok(()) => {
                        toast::neutral(&store, t(store.language, Key::note_forgotten));
                        notes.set((*notes).iter().filter(|n| n.id != id).cloned().collect());
                        results.set(
                            (*results)
                                .iter()
                                .filter(|h| h.kind != "knowledge" || h.ref_id != id)
                                .cloned()
                                .collect(),
                        );
                    }
                    Err(e) => toast::error(
                        &store,
                        t(store.language, Key::forget_failed),
                        Some(e.user_message()),
                    ),
                }
            });
        })
    };

    // ---- Cloud AI escalation --------------------------------------------
    let ai_settings = ai::AiSettings::load();
    let provider = ai_settings.text_provider();

    let ask_consent = {
        let ask = ask.clone();
        Callback::from(move |_: MouseEvent| ask.set(Some(Ask::Consent)))
    };
    let ask_cancel = {
        let ask = ask.clone();
        Callback::from(move |_: MouseEvent| ask.set(None))
    };
    // The generation half of RAG, shared by the consent card's Send and the
    // quick bar's auto-ask.
    let fire_ask = {
        let ask = ask.clone();
        let query = query.clone();
        let results = results.clone();
        let settings = ai_settings.clone();
        Callback::from(move |_: ()| {
            let Some(provider) = settings.text_provider() else {
                return;
            };
            let key = settings.key_for(provider).unwrap_or_default().to_owned();
            let question = (*query).clone();
            let passages: Vec<String> = (*results)
                .iter()
                .take(ASK_PASSAGES)
                .enumerate()
                .map(|(i, h)| format!("[{}] {}", i + 1, h.text))
                .collect();
            let ask = ask.clone();
            ask.set(Some(Ask::Busy));
            wasm_bindgen_futures::spawn_local(async move {
                const SYSTEM: &str = "You answer questions using ONLY the numbered context \
                    passages provided. Cite passage numbers like [2] when you use them. If the \
                    passages do not contain the answer, say so plainly. Answer in the language \
                    of the question. Be concise.";
                let user = format!(
                    "Context passages:\n{}\n\nQuestion: {}",
                    passages.join("\n"),
                    question
                );
                match ai::generate_text(provider, &key, SYSTEM, &user).await {
                    Ok(answer) => ask.set(Some(Ask::Answered(answer))),
                    Err(e) => ask.set(Some(Ask::Failed(e))),
                }
            });
        })
    };
    let ask_send = {
        let fire_ask = fire_ask.clone();
        Callback::from(move |_: MouseEvent| fire_ask.emit(()))
    };

    // The quick bar's auto-ask: once the seeded retrieval has landed, answer.
    {
        let auto_ask = auto_ask.clone();
        let fire_ask = fire_ask.clone();
        let n = results.len();
        let done = *searched;
        use_effect_with((n, done), move |_| {
            if done && *auto_ask {
                auto_ask.set(false);
                if n > 0 {
                    fire_ask.emit(());
                }
            }
            || ()
        });
    }

    let hit_count = results.len().min(ASK_PASSAGES);
    let visible_notes = filtered_notes(&notes, &note_filter);

    html! {
        <>
        <div class="topcoat-navigation-bar">
            <Back onclick={{
                let on_navigate = p.on_navigate.clone();
                Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Rooms))
            }} />
            <h1 class="topcoat-navigation-bar__title">{ t(lang, Key::nav_knowledge) }</h1>
        </div>
        <div class="fn-scroll fn-knowledge">
            <p class="fn-muted fn-knowledge__tagline">{ t(lang, Key::knowledge_tagline) }</p>

            <div class="fn-tabs fn-knowledge__tabs" role="tablist"
                 aria-label={t(lang, Key::nav_knowledge)}>
                { mode_tab(lang, Key::mode_search, Mode::Search, &mode) }
                { mode_tab(lang, Key::mode_teach, Mode::Teach, &mode) }
            </div>

            if *mode == Mode::Search {
                <div class="fn-knowledge__searchrow">
                    <input
                        class="topcoat-text-input fn-grow"
                        type="search"
                        placeholder={t(lang, Key::search_everything)}
                        aria-label={t(lang, Key::search_everything)}
                        value={(*query).clone()}
                        oninput={on_query_input}
                        onkeydown={on_query_key}
                    />
                    <button
                        type="button"
                        class="topcoat-button--cta"
                        onclick={{
                            let query = query.clone();
                            let run_search = run_search.clone();
                            Callback::from(move |_: MouseEvent| run_search.emit((*query).clone()))
                        }}
                    >
                        if *busy { { t(lang, Key::searching) } } else { { icons::search(16) } }
                    </button>
                </div>

                if !tags.is_empty() {
                    <div class="fn-knowledge__tags" aria-label={t(lang, Key::browse_by_tag)}>
                        { for tags.iter().map(|tc| {
                            let pick_tag = pick_tag.clone();
                            let tag = tc.tag.clone();
                            html! {
                                <button
                                    type="button"
                                    class="fn-tagchip"
                                    onclick={Callback::from(move |_: MouseEvent| pick_tag.emit(tag.clone()))}
                                >
                                    { format!("#{}", tc.tag) }
                                    <span class="fn-nums fn-tagchip__count">{ tc.count }</span>
                                </button>
                            }
                        }) }
                    </div>
                }

                // The AI escalation rail — only with a configured provider,
                // only with results to escalate.
                if let (Some(pr), false) = (provider, results.is_empty()) {
                    { ask_rail(lang, pr, hit_count, &ask, ask_consent.clone(),
                               ask_cancel.clone(), ask_send.clone()) }
                }

                if *busy && results.is_empty() {
                    <div class="fn-knowledge__busy"><span class="fn-spinner" aria-hidden="true" /></div>
                } else if *searched && results.is_empty() {
                    <Empty
                        art="🔭"
                        art_class={classes!("fn-art--knowledge")}
                        title={t(lang, Key::no_results)}
                        description={t(lang, Key::no_results_hint)}
                    />
                } else if !*searched && results.is_empty() {
                    <Empty
                        art="🧠"
                        art_class={classes!("fn-art--knowledge")}
                        title={t(lang, Key::nav_knowledge)}
                        description={t(lang, Key::knowledge_intro)}
                    />
                } else {
                    <ul class="fn-knowledge__results">
                        { for results.iter().map(|hit| result_row(
                            lang, hit, &store, &me, now,
                            &p.on_navigate, &pick_tag, &forget,
                        )) }
                    </ul>
                }
            } else {
                <div class="fn-knowledge__teach">
                    <textarea
                        class="topcoat-textarea fn-knowledge__draft"
                        rows="4"
                        placeholder={t(lang, Key::teach_placeholder)}
                        aria-label={t(lang, Key::mode_teach)}
                        value={(*draft).clone()}
                        oninput={{
                            let draft = draft.clone();
                            Callback::from(move |e: InputEvent| {
                                if let Some(el) = e.target_dyn_into::<web_sys::HtmlTextAreaElement>() {
                                    draft.set(el.value());
                                }
                            })
                        }}
                    />
                    <p class="fn-knowledge__hint">{ t(lang, Key::teach_hint) }</p>
                    <BusyButton
                        busy={*teaching}
                        disabled={draft.trim().is_empty()}
                        label={t(lang, Key::mode_teach)}
                        onclick={teach_now}
                    />

                    // Filter over what has been taught (finding the note you
                    // meant to remove must not require scrolling), plus a
                    // fetch-from-server button for what others taught since.
                    <div class="fn-knowledge__searchrow">
                        <input
                            class="topcoat-text-input fn-grow"
                            type="search"
                            placeholder={t(lang, Key::filter_notes)}
                            aria-label={t(lang, Key::filter_notes)}
                            value={(*note_filter).clone()}
                            oninput={{
                                let note_filter = note_filter.clone();
                                Callback::from(move |e: InputEvent| {
                                    if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                        note_filter.set(el.value());
                                    }
                                })
                            }}
                        />
                        <button
                            type="button"
                            class="topcoat-button"
                            aria-label={t(lang, Key::sync_now)}
                            title={t(lang, Key::sync_now)}
                            disabled={*refreshing}
                            onclick={reload_notes}
                        >
                            if *refreshing {
                                <span class="fn-spinner" aria-hidden="true" />
                            } else {
                                { icons::refresh(16) }
                            }
                        </button>
                    </div>
                    if notes.is_empty() {
                        <Empty
                            art="🌱"
                            art_class={classes!("fn-art--knowledge")}
                            title={t(lang, Key::knowledge_empty)}
                            description={t(lang, Key::teach_hint)}
                        />
                    } else {
                        if visible_notes.is_empty() {
                            <Empty
                                art="🔭"
                                title={t(lang, Key::no_matching_notes)}
                            />
                        } else {
                            <ul class="fn-knowledge__results">
                                { for visible_notes.iter().map(|note|
                                    note_row(lang, note, &me, now, &forget, &pick_tag)) }
                            </ul>
                        }
                    }
                </div>
            }
        </div>
        </>
    }
}

fn mode_tab(lang: crate::i18n::Lang, key: Key, this: Mode, mode: &UseStateHandle<Mode>) -> Html {
    let selected = **mode == this;
    let onclick = {
        let mode = mode.clone();
        Callback::from(move |_: MouseEvent| mode.set(this))
    };
    html! {
        <button
            type="button"
            class="fn-tab"
            role="tab"
            aria-selected={selected.to_string()}
            {onclick}
        >{ t(lang, key) }</button>
    }
}

/// The consent-gated AI answer rail (docs/SEARCH.md §5). The provider is
/// named on the button; the consent card enumerates what leaves the server.
#[allow(clippy::too_many_arguments)]
fn ask_rail(
    lang: crate::i18n::Lang,
    provider: ai::Provider,
    passages: usize,
    ask: &UseStateHandle<Option<Ask>>,
    open: Callback<MouseEvent>,
    cancel: Callback<MouseEvent>,
    send: Callback<MouseEvent>,
) -> Html {
    match &**ask {
        None => html! {
            <div class="fn-askrail">
                <button type="button" class="topcoat-button" onclick={open}>
                    { icons::spark(16) }
                    { " " }
                    { t(lang, Key::ask_ai_with_results).replace("{provider}", provider.label()) }
                </button>
            </div>
        },
        Some(Ask::Consent) => html! {
            <div class="fn-askrail fn-askrail--consent" role="alertdialog"
                 aria-label={t(lang, Key::ask_consent_title)}>
                <strong>{ t(lang, Key::ask_consent_title) }</strong>
                <p>{ t(lang, Key::ask_consent_body)
                        .replace("{n}", &passages.to_string())
                        .replace("{provider}", provider.label()) }</p>
                <div class="fn-row">
                    <button type="button" class="topcoat-button--cta" onclick={send}>
                        { t(lang, Key::ask_send) }
                    </button>
                    <button type="button" class="topcoat-button--quiet" onclick={cancel}>
                        { t(lang, Key::cancel) }
                    </button>
                </div>
            </div>
        },
        Some(Ask::Busy) => html! {
            <div class="fn-askrail">
                <span class="fn-spinner" aria-hidden="true" />
                { " " }
                { t(lang, Key::asking_ai) }
            </div>
        },
        Some(Ask::Answered(answer)) => html! {
            <div class="fn-askrail fn-askrail--answer">
                <div class="fn-askrail__meta">
                    { icons::spark(14) }
                    { " " }
                    { t(lang, Key::ai_answered_from)
                        .replace("{provider}", provider.label())
                        .replace("{n}", &passages.to_string()) }
                </div>
                <p class="fn-askrail__text">{ answer }</p>
                <button type="button" class="topcoat-button--quiet" onclick={cancel}>
                    { t(lang, Key::close) }
                </button>
            </div>
        },
        Some(Ask::Failed(e)) => html! {
            <div class="fn-askrail fn-askrail--error">
                { t(lang, Key::ai_ask_failed) }{ ": " }{ e }
                <button type="button" class="topcoat-button--quiet" onclick={cancel}>
                    { t(lang, Key::close) }
                </button>
            </div>
        },
    }
}

/// One search hit: kind badge, text with clickable tags, provenance line.
#[allow(clippy::too_many_arguments)]
fn result_row(
    lang: crate::i18n::Lang,
    hit: &SearchHit,
    store: &state::Store,
    me: &Option<WalletAddress>,
    now: i64,
    on_navigate: &Callback<Route>,
    pick_tag: &Callback<String>,
    forget: &Callback<String>,
) -> Html {
    let is_knowledge = hit.kind == "knowledge";
    let room = hit
        .room_id
        .as_deref()
        .and_then(|id| store.rooms.iter().find(|r| r.room.id.as_str() == id));
    let room_name = room.map(|r| r.room.name.clone());
    let open = {
        let on_navigate = on_navigate.clone();
        let room_id = hit.room_id.clone().and_then(|id| RoomId::new(&id).ok());
        Callback::from(move |_: MouseEvent| {
            if let Some(id) = room_id.clone() {
                on_navigate.emit(Route::Room(id));
            }
        })
    };
    let mine = matches!((me, &hit.sender), (Some(me), Some(s)) if me.as_str() == s);
    let sender = hit.sender.as_ref().and_then(|s| WalletAddress::new(s).ok());

    html! {
        <li class="fn-hitcard" key={format!("{}:{}", hit.kind, hit.ref_id)}>
            <div class="fn-hitcard__meta">
                if is_knowledge {
                    <span class="fn-hitcard__kind fn-hitcard__kind--knowledge">
                        { "📚 " }{ t(lang, Key::nav_knowledge) }
                    </span>
                } else if let Some(name) = room_name {
                    <button type="button" class="fn-hitcard__kind" onclick={open.clone()}
                            title={t(lang, Key::open_in_room)}>
                        { "💬 " }{ name }
                    </button>
                }
                if let Some(addr) = sender {
                    <Addr address={addr} />
                }
                <time class="fn-hitcard__time">
                    { format::relative_time(hit.timestamp, now) }
                </time>
            </div>
            <div class="fn-hitcard__text">{ tagged_text(&hit.text, pick_tag) }</div>
            if is_knowledge && mine {
                <button
                    type="button"
                    class="topcoat-button--quiet fn-hitcard__forget"
                    onclick={{
                        let forget = forget.clone();
                        let id = hit.ref_id.clone();
                        Callback::from(move |_: MouseEvent| forget.emit(id.clone()))
                    }}
                >{ t(lang, Key::forget_note) }</button>
            } else if !is_knowledge {
                <button type="button" class="topcoat-button--quiet fn-hitcard__forget"
                        onclick={open}>
                    { t(lang, Key::open_in_room) }
                </button>
            }
        </li>
    }
}

/// Case-insensitive substring filter over content, tags, and owner — the
/// cheap local sieve for "which one did I mean to remove".
fn filtered_notes(notes: &[KnowledgeNote], filter: &str) -> Vec<KnowledgeNote> {
    let needle = filter.trim().trim_start_matches('#').to_lowercase();
    if needle.is_empty() {
        return notes.to_vec();
    }
    notes
        .iter()
        .filter(|n| {
            n.content.to_lowercase().contains(&needle)
                || n.tags.iter().any(|t| t.contains(&needle))
                || n.owner_address.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect()
}

/// One taught note in the Teach tab. Your own notes carry the `You` badge —
/// ownership is what decides whether Forget appears, so it must be legible
/// at a glance, not discovered by hunting for the button.
fn note_row(
    lang: crate::i18n::Lang,
    note: &KnowledgeNote,
    me: &Option<WalletAddress>,
    now: i64,
    forget: &Callback<String>,
    pick_tag: &Callback<String>,
) -> Html {
    let mine = me
        .as_ref()
        .is_some_and(|m| m.as_str() == note.owner_address);
    let owner = WalletAddress::new(&note.owner_address).ok();
    html! {
        <li class="fn-hitcard" key={note.id.clone()}>
            <div class="fn-hitcard__meta">
                <span class="fn-hitcard__kind fn-hitcard__kind--knowledge">
                    { "📚 " }{ t(lang, Key::nav_knowledge) }
                </span>
                if mine {
                    <Badge variant="self">{ t(lang, Key::you) }</Badge>
                }
                if let Some(addr) = owner {
                    <Addr address={addr} />
                }
                <time class="fn-hitcard__time">{ format::relative_time(note.created_at, now) }</time>
            </div>
            <div class="fn-hitcard__text">{ tagged_text(&note.content, pick_tag) }</div>
            if mine {
                <button
                    type="button"
                    class="topcoat-button--quiet fn-hitcard__forget"
                    onclick={{
                        let forget = forget.clone();
                        let id = note.id.clone();
                        Callback::from(move |_: MouseEvent| forget.emit(id.clone()))
                    }}
                >{ t(lang, Key::forget_note) }</button>
            }
        </li>
    }
}

/// Render text with its `#tags` as clickable chips; everything else escapes
/// as plain text nodes, same rule as the chat bubble.
fn tagged_text(text: &str, pick_tag: &Callback<String>) -> Html {
    let mut out: Vec<Html> = Vec::new();
    for (i, token) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(html! { { " " } });
        }
        match hashtag_of(token) {
            Some(tag) => {
                let pick_tag = pick_tag.clone();
                let label = token.to_owned();
                out.push(html! {
                    <button
                        type="button"
                        class="fn-taglink"
                        onclick={Callback::from(move |_: MouseEvent| pick_tag.emit(tag.clone()))}
                    >{ label }</button>
                });
            }
            None => out.push(html! { { token } }),
        }
    }
    html! { <>{ for out }</> }
}

/// The client-side mirror of the server's hashtag rule (docs/SEARCH.md §1):
/// `#` + letters/digits/`_`/`-`, at least one letter, lowercased. Trailing
/// punctuation (`#rust!`) is tolerated by stripping non-tag chars first.
pub fn hashtag_of(token: &str) -> Option<String> {
    let rest = token.strip_prefix('#')?;
    let tag: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .flat_map(char::to_lowercase)
        .collect();
    if tag.is_empty() || !tag.chars().any(char::is_alphabetic) || tag.chars().count() > 64 {
        return None;
    }
    Some(tag)
}

#[cfg(test)]
mod tests {
    use super::{filtered_notes, hashtag_of, KnowledgeNote};

    #[test]
    fn tags_match_the_server_rule() {
        assert_eq!(hashtag_of("#Rust"), Some("rust".into()));
        assert_eq!(hashtag_of("#김치"), Some("김치".into()));
        assert_eq!(hashtag_of("#rust!"), Some("rust".into()));
        assert_eq!(hashtag_of("#123"), None);
        assert_eq!(hashtag_of("#"), None);
        assert_eq!(hashtag_of("plain"), None);
    }

    fn note(id: &str, owner: &str, content: &str, tags: &[&str]) -> KnowledgeNote {
        KnowledgeNote {
            id: id.to_owned(),
            owner_address: owner.to_owned(),
            content: content.to_owned(),
            room_id: None,
            source_message_id: None,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn world() -> Vec<KnowledgeNote> {
        vec![
            note(
                "k1",
                "0xAAA",
                "The spare key hangs by the door #home",
                &["home"],
            ),
            note(
                "k2",
                "0xBBB",
                "홍콩 최고 식당은 홍콩 대학교 맥도날드이다",
                &[],
            ),
            note(
                "k3",
                "0xAAA",
                "Backup runs nightly at 03:00 #ops #home",
                &["ops", "home"],
            ),
        ]
    }

    fn ids(notes: &[KnowledgeNote]) -> Vec<&str> {
        notes.iter().map(|n| n.id.as_str()).collect()
    }

    #[test]
    fn an_empty_or_blank_filter_keeps_everything() {
        assert_eq!(ids(&filtered_notes(&world(), "")), ["k1", "k2", "k3"]);
        assert_eq!(ids(&filtered_notes(&world(), "   ")), ["k1", "k2", "k3"]);
    }

    #[test]
    fn content_matches_case_insensitively() {
        assert_eq!(ids(&filtered_notes(&world(), "SPARE key")), ["k1"]);
        assert_eq!(ids(&filtered_notes(&world(), "backup")), ["k3"]);
    }

    #[test]
    fn korean_content_is_findable() {
        assert_eq!(ids(&filtered_notes(&world(), "맥도날드")), ["k2"]);
    }

    #[test]
    fn a_tag_matches_with_or_without_the_hash() {
        assert_eq!(ids(&filtered_notes(&world(), "#ops")), ["k3"]);
        assert_eq!(ids(&filtered_notes(&world(), "ops")), ["k3"]);
        // A tag shared by two notes returns both, newest ordering preserved.
        assert_eq!(ids(&filtered_notes(&world(), "#home")), ["k1", "k3"]);
    }

    #[test]
    fn an_owner_address_narrows_to_their_notes() {
        assert_eq!(ids(&filtered_notes(&world(), "0xaaa")), ["k1", "k3"]);
        assert_eq!(ids(&filtered_notes(&world(), "0xBBB")), ["k2"]);
    }

    #[test]
    fn a_hopeless_filter_matches_nothing_rather_than_everything() {
        assert!(filtered_notes(&world(), "zzz-not-there").is_empty());
    }
}
