//! One message row: bubble, ledger gutter, reactions and the hover tools
//! (DESIGN.md §7.2, §7.5).
//!
//! The **ledger gutter** is the element this product is remembered by: an
//! 8-character `msgHash` slug in mono under every bubble, at 55 % opacity until
//! hover. When the hash has been anchored on-chain it turns emerald and gains a
//! check. No other messenger has a receipt stub, and it is the visible reason
//! the protocol hashes every message.

use pocketskynet_core::{MessageId, WalletAddress};
use yew::prelude::*;

use crate::api::{BlockchainInfo, Message};
use crate::crypto::Decrypted;
use crate::format;

use super::common::{Addr, Badge, Ident, IdentSize, Popover};
use super::composer::Picker;
use super::icons;
use crate::i18n::{t, Key, Lang};

#[derive(Properties, PartialEq)]
pub struct MessageProps {
    pub message: Message,
    pub body: Decrypted,
    pub is_own: bool,
    /// Suppress the avatar and sender header (same sender, within five
    /// minutes, same day).
    pub grouped: bool,
    /// The room is encrypted, so a *plaintext* message inside it is worth
    /// flagging.
    pub room_encrypted: bool,
    pub reactions: Vec<(String, Vec<WalletAddress>)>,
    pub me: WalletAddress,
    pub chain: BlockchainInfo,
    pub tz: i32,
    pub on_react: Callback<(MessageId, String, bool)>,
    pub on_copy: Callback<String>,
    /// Carries the readable text alongside the id so the confirm dialog can
    /// quote the message it is about to destroy.
    pub on_delete: Callback<(MessageId, Option<String>)>,
    pub on_edit: Callback<(MessageId, String)>,
    pub on_open_picker: Callback<MessageId>,
    /// The reaction picker is open *on this row*. The picker renders inside
    /// the tools rail so it appears at the message, not over the composer —
    /// the pointer is already here when it opens.
    pub picker_open: bool,
    pub on_close_picker: Callback<()>,
    /// A `#tag` chip was clicked or "Teach from this message" chosen — the
    /// parent seeds the Knowledge page and navigates there (docs/SEARCH.md §5).
    pub on_knowledge: Callback<crate::state::KnowledgeSeed>,
}

#[function_component(MessageRow)]
pub fn message_row(p: &MessageProps) -> Html {
    let store = crate::state::use_store();
    let lang = crate::state::use_store().language;
    let editing = use_state(|| false);
    let draft = use_state(|| p.body.text().unwrap_or_default().to_owned());
    let menu_open = use_state(|| false);

    let m = &p.message;
    let sender_name = m
        .sender
        .as_ref()
        .map(|s| s.display_name())
        .unwrap_or_else(|| m.sender_address.abbreviated());

    let mut class = classes!("fn-msg");
    if p.is_own {
        class.push("fn-msg--own");
    }
    if p.grouped {
        class.push("fn-msg--grouped");
    }
    // `.fn-msg` animates in with `fill-mode: both`, which leaves it a stacking
    // context for good — so the menu's own z-index is scoped *inside* this
    // message and every later message paints over it. Raising the whole
    // article while its menu or reaction picker is open is what actually
    // lifts the popover, rather than raising the popover within a box it
    // cannot escape.
    if *menu_open || p.picker_open {
        class.push("fn-msg--menu-open");
    }
    // Disintegrating (app.css §7). Set from the store rather than locally so
    // the effect survives this component re-rendering mid-animation.
    if store.dissolving.contains(&m.id) {
        class.push("fn-msg--dissolving");
    }

    let copy_hash = {
        let on_copy = p.on_copy.clone();
        let hash = m.msg_hash.clone();
        Callback::from(move |_: MouseEvent| on_copy.emit(hash.clone()))
    };

    let toggle_menu = {
        let menu_open = menu_open.clone();
        Callback::from(move |_: MouseEvent| menu_open.set(!*menu_open))
    };

    let start_edit = {
        let editing = editing.clone();
        let draft = draft.clone();
        let menu_open = menu_open.clone();
        let text = p.body.text().unwrap_or_default().to_owned();
        Callback::from(move |_: MouseEvent| {
            draft.set(text.clone());
            editing.set(true);
            menu_open.set(false);
        })
    };

    let commit_edit = {
        let editing = editing.clone();
        let draft = draft.clone();
        let on_edit = p.on_edit.clone();
        let id = m.id.clone();
        Callback::from(move |_: ()| {
            let text = draft.trim().to_owned();
            if !text.is_empty() {
                on_edit.emit((id.clone(), text));
            }
            editing.set(false);
        })
    };

    html! {
        <article {class} data-id={m.id.to_string()}>
            if !p.is_own && !p.grouped {
                <Ident
                    seed={m.sender_address.to_string()}
                    class="fn-msg__avatar"
                    size={IdentSize::Md}
                    image={m.sender.as_ref().and_then(|u| u.profile_image.clone())}
                    zoom={crate::components::common::Zoom {
                        title: sender_name.clone(),
                        subtitle: Some(m.sender_address.to_checksummed()),
                        copy: Some(m.sender_address.to_checksummed()),
                    }}
                />
            }

            if !p.grouped {
                <header class="fn-msg__sender">
                    <strong>{ &sender_name }</strong>
                    <Addr address={m.sender_address.clone()} />
                    <time class="fn-msg__time" datetime={m.created_at.clone().unwrap_or_default()}>
                        { format::hhmm(m.message_timestamp, p.tz) }
                    </time>
                </header>
            }

            if *editing {
                { edit_box(lang, &draft, commit_edit.clone(), editing.clone()) }
            } else {
                { bubble(lang, p) }
            }

            <footer class="fn-msg__foot">
                if p.room_encrypted && !m.is_encrypted {
                    // In an encrypted room every message should be encrypted;
                    // one that is not is worth calling out. (The converse — a
                    // per-message lock on every bubble — is deliberately not
                    // drawn: repeating it 200 times devalues the signal.)
                    <Badge variant="danger">{ t(lang, Key::not_encrypted) }</Badge>
                }
                if m.is_edited() {
                    <span>{ t(lang, Key::edited) }</span>
                }
                { hash_slug(lang, m, &p.chain, copy_hash) }
            </footer>

            if !p.reactions.is_empty() {
                <div class="fn-reactions">
                    { for p.reactions.iter().map(|(code, who)| {
                        reaction_chip(&m.id, code, who, &p.me, &p.on_react)
                    }) }
                </div>
            }

            <div class="fn-msg__tools">
                <button
                    type="button"
                    class="topcoat-icon-button--quiet"
                    aria-label={t(lang, Key::react_to_message).replace("{name}", &sender_name)}
                    onclick={{
                        let on_open_picker = p.on_open_picker.clone();
                        let id = m.id.clone();
                        Callback::from(move |_: MouseEvent| on_open_picker.emit(id.clone()))
                    }}
                >
                    { icons::smile(16) }
                </button>
                <button
                    type="button"
                    class="topcoat-icon-button--quiet"
                    aria-label={t(lang, Key::more_actions_for).replace("{name}", &sender_name)}
                    aria-expanded={menu_open.to_string()}
                    onclick={toggle_menu}
                >
                    { icons::more(16) }
                </button>
                // Unconditional: `Popover` must observe `open` going false
                // to run its exit (common.rs).
                { menu(lang, p, start_edit, *menu_open, menu_open.clone()) }
                { reaction_picker(p) }
            </div>
        </article>
    }
}

/// The bubble body, or one of the three distinct sealed placeholders.
fn bubble(lang: Lang, p: &MessageProps) -> Html {
    let on_tag = {
        let on_knowledge = p.on_knowledge.clone();
        Callback::from(move |tag: String| {
            on_knowledge.emit(crate::state::KnowledgeSeed::Search {
                query: format!("#{tag}"),
                ask: false,
            });
        })
    };
    match &p.body {
        Decrypted::Plaintext(text) | Decrypted::Text(text) => html! {
            <div class="fn-bubble">{ render_content(lang, text, &on_tag) }</div>
        },
        sealed => html! {
            // Mono and muted, and never collapsed into one string: "no key for
            // this epoch", "missing metadata" and "decryption failed" have
            // different causes and different remedies.
            <div class="fn-bubble fn-bubble--sealed">
                { sealed.placeholder().unwrap_or_default() }
            </div>
        },
    }
}

/// Autolink `http(s)` URLs; render bare image/GIF URLs as the image itself
/// and YouTube URLs as an embed; everything else is plain text (DESIGN.md
/// §7.2 "Content").
///
/// No markdown and no HTML. Message content is entirely attacker-chosen, and
/// Yew's text nodes escape it — introducing a markdown renderer would mean
/// introducing an HTML sanitiser, and the wrong sanitiser is an XSS. An
/// `<img src>` / `youtube-nocookie` iframe carries no script capability, so
/// inlining media stays inside that rule.
fn render_content(lang: Lang, text: &str, on_tag: &Callback<String>) -> Html {
    let mut out: Vec<Html> = Vec::new();
    for (i, token) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(html! { { " " } });
        }
        if let Some(id) = crate::api::attachment_id_in(token) {
            // An attachment posted into the room. Rendered as a card rather
            // than as the path it literally is, but the path stays visible
            // inside the card — see `AttachmentEmbed`.
            out.push(html! { <AttachmentEmbed id={id.to_owned()} /> });
        } else if token.starts_with("https://") || token.starts_with("http://") {
            if let Some(id) = youtube_id(token) {
                out.push(youtube_embed(lang, &id));
            } else if is_image_url(token) {
                out.push(html! { <ImageEmbed url={token.to_owned()} /> });
            } else {
                out.push(html! {
                    <a href={token.to_owned()} target="_blank" rel="noopener noreferrer">{ token }</a>
                });
            }
        } else if let Some(tag) = super::knowledge::hashtag_of(token) {
            // A live filter, not decoration: the chip opens Knowledge
            // scoped to this tag (docs/SEARCH.md §5).
            let on_tag = on_tag.clone();
            let label = token.to_owned();
            out.push(html! {
                <button
                    type="button"
                    class="fn-taglink"
                    onclick={Callback::from(move |_: MouseEvent| on_tag.emit(tag.clone()))}
                >{ label }</button>
            });
        } else {
            out.push(html! { { token } });
        }
    }
    html! { <>{ for out }</> }
}

/// `(host, path, query)` of an `http(s)` URL, host lowercased, no fragment.
fn split_url(url: &str) -> Option<(String, String, &str)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let (host_path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (host, path) = match host_path.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (host_path, String::new()),
    };
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    Some((host.to_ascii_lowercase(), path, query))
}

/// Image by file extension, or by a host that only ever serves images.
///
/// Suffix-matched against the hostname, not `contains` — `imgur.com.evil.example`
/// must not qualify.
fn is_image_url(url: &str) -> bool {
    const EXTENSIONS: [&str; 8] = [
        ".jpg", ".jpeg", ".png", ".webp", ".bmp", ".svg", ".avif", ".gif",
    ];
    const IMAGE_HOSTS: [&str; 6] = [
        "giphy.com",
        "tenor.com",
        "imgur.com",
        "images.unsplash.com",
        "images.pexels.com",
        "twimg.com",
    ];
    let Some((host, path, _)) = split_url(url) else {
        return false;
    };
    let path = path.to_ascii_lowercase();
    EXTENSIONS.iter().any(|ext| path.ends_with(ext))
        || IMAGE_HOSTS
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// The 11-character video id, from any of the YouTube URL shapes.
fn youtube_id(url: &str) -> Option<String> {
    fn valid(id: &str) -> bool {
        id.len() == 11
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
    let (host, path, query) = split_url(url)?;
    let id = if host == "youtu.be" {
        path.trim_start_matches('/').split('/').next()?.to_owned()
    } else if host == "youtube.com" || host.ends_with(".youtube.com") {
        if path == "/watch" {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix("v="))?
                .to_owned()
        } else {
            let rest = path
                .strip_prefix("/embed/")
                .or_else(|| path.strip_prefix("/shorts/"))?;
            rest.split('/').next()?.to_owned()
        }
    } else {
        return None;
    };
    valid(&id).then_some(id)
}

fn youtube_embed(lang: Lang, id: &str) -> Html {
    html! {
        <span class="fn-media fn-media--video">
            <iframe
                src={format!("https://www.youtube-nocookie.com/embed/{id}")}
                title={t(lang, Key::youtube_video)}
                allow="accelerometer; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
                allowfullscreen=true
            />
        </span>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MediaLoad {
    Loading,
    Loaded,
    Failed,
}

#[derive(Properties, PartialEq)]
struct AttachmentEmbedProps {
    id: AttrValue,
}

/// An attachment inside a message.
///
/// Everything here follows from one constraint: `/api/files/{id}/raw` requires a
/// bearer token, so unlike `ImageEmbed` this cannot simply point an `<img src>`
/// at a URL. The metadata is fetched, and for an image or a video the bytes are
/// fetched too and wrapped in an object URL. That is also why a preview costs
/// the whole file — there is no Range support anywhere in this server, so a
/// video cannot be streamed, only downloaded and then played.
///
/// The object URL is revoked on unmount. Without that, scrolling a room with
/// twenty videos pins twenty copies of them for the life of the document.
#[function_component(AttachmentEmbed)]
fn attachment_embed(p: &AttachmentEmbedProps) -> Html {
    let store = crate::state::use_store();
    let lang = store.language;
    let meta = use_state(|| Option::<crate::api::FileMeta>::None);
    let blob = use_state(|| Option::<String>::None);
    let load = use_state(|| MediaLoad::Loading);

    {
        let store = store.clone();
        let meta = meta.clone();
        let blob = blob.clone();
        let load = load.clone();
        let id = p.id.to_string();
        use_effect_with(id.clone(), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                let Ok(file) = store.client.file(&id).await else {
                    load.set(MediaLoad::Failed);
                    return;
                };
                let previewable = file.is_previewable_image() || file.is_previewable_video();
                let mime = file.preview_mime();
                meta.set(Some(file));
                if !previewable {
                    // Nothing to fetch: the card is the whole render.
                    load.set(MediaLoad::Loaded);
                    return;
                }
                match store.client.download_file(&id).await {
                    Ok(bytes) => match super::common::object_url(&bytes, mime) {
                        Some(url) => {
                            blob.set(Some(url));
                            load.set(MediaLoad::Loaded);
                        }
                        None => load.set(MediaLoad::Failed),
                    },
                    Err(_) => load.set(MediaLoad::Failed),
                }
            });
            || ()
        });
    }

    {
        let blob = blob.clone();
        use_effect_with((), move |_| {
            move || {
                if let Some(url) = blob.as_deref() {
                    let _ = web_sys::Url::revoke_object_url(url);
                }
            }
        });
    }

    if *load == MediaLoad::Failed {
        return html! {
            <span class="fn-media--failed">{ t(lang, Key::attachment_failed) }</span>
        };
    }
    let Some(file) = (*meta).clone() else {
        return html! {
            <span class="fn-attach fn-attach--loading">
                <span class="fn-spinner" aria-hidden="true"></span>
            </span>
        };
    };

    // Open in a real new window rather than navigating: the person is in the
    // middle of a conversation, and a preview should never cost them their
    // place in it. `noopener` because the blob document has no business
    // reaching back into this one.
    let open = {
        let blob = blob.clone();
        Callback::from(move |_: MouseEvent| {
            if let (Some(win), Some(url)) = (web_sys::window(), blob.as_deref()) {
                let _ = win.open_with_url_and_target_and_features(url, "_blank", "noopener");
            }
        })
    };

    let save = {
        let store = store.clone();
        let file = file.clone();
        Callback::from(move |_: MouseEvent| {
            let store = store.clone();
            let file = file.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(bytes) = store.client.download_file(&file.id).await {
                    if let Some(url) = super::common::object_url(&bytes, "application/octet-stream")
                    {
                        super::common::save_as(&url, &file.filename);
                        let _ = web_sys::Url::revoke_object_url(&url);
                    }
                }
            });
        })
    };

    let media = match (&*blob, file.is_previewable_video()) {
        (Some(url), true) => html! {
            // `controls` and no autoplay: an attachment that starts making
            // noise when it scrolls into view is a hostile attachment.
            <video class="fn-attach__media" src={url.clone()} controls=true preload="metadata" />
        },
        (Some(url), false) => html! {
            <button
                type="button"
                class="fn-attach__shot"
                aria-label={t(lang, Key::open_in_new_window)}
                title={t(lang, Key::open_in_new_window)}
                onclick={open.clone()}
            >
                <img class="fn-attach__media" src={url.clone()} alt={file.filename.clone()} />
            </button>
        },
        (None, _) => Html::default(),
    };

    html! {
        <span class="fn-attach">
            { media }
            <span class="fn-attach__row">
                <span class="fn-attach__plate" aria-hidden="true">
                    if file.extension().is_empty() {
                        { icons::files(16) }
                    } else {
                        <span class="fn-attach__ext">{ file.extension().to_uppercase() }</span>
                    }
                </span>
                <span class="fn-attach__body">
                    <span class="fn-attach__name fn-truncate">{ &file.filename }</span>
                    <span class="fn-attach__meta fn-nums">{ file.human_size() }</span>
                    // The full path, verbatim. Selectable text rather than a
                    // link: it needs a bearer token, so an anchor would 401 and
                    // teach the wrong thing about how attachments work.
                    <code class="fn-attach__path">{ &file.url }</code>
                </span>
                <span class="fn-attach__tools">
                    if blob.is_some() {
                        <button
                            type="button"
                            class="topcoat-icon-button--quiet"
                            aria-label={t(lang, Key::open_in_new_window)}
                            title={t(lang, Key::open_in_new_window)}
                            onclick={open}
                        >{ icons::external(16) }</button>
                    }
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::file_download)}
                        title={t(lang, Key::file_download)}
                        onclick={save}
                    >{ icons::download(16) }</button>
                </span>
            </span>
        </span>
    }
}

#[derive(Properties, PartialEq)]
struct ImageEmbedProps {
    url: AttrValue,
}

/// An inline image: lazy, a spinner where the image will land, and a failure
/// row that keeps the URL clickable (DESIGN.md §7.2 "Content").
#[function_component(ImageEmbed)]
fn image_embed(p: &ImageEmbedProps) -> Html {
    let lang = crate::state::use_store().language;
    let load = use_state(|| MediaLoad::Loading);

    if *load == MediaLoad::Failed {
        return html! {
            <span class="fn-media--failed">
                { t(lang, Key::image_failed) }
                { " " }
                <a href={p.url.clone()} target="_blank" rel="noopener noreferrer">{ &p.url }</a>
            </span>
        };
    }

    let onload = {
        let load = load.clone();
        Callback::from(move |_: Event| load.set(MediaLoad::Loaded))
    };
    let onerror = {
        let load = load.clone();
        Callback::from(move |_: Event| load.set(MediaLoad::Failed))
    };
    let mut class = classes!("fn-media");
    if *load == MediaLoad::Loading {
        class.push("fn-media--loading");
    }
    html! {
        <span {class}>
            if *load == MediaLoad::Loading {
                <span class="fn-spinner" aria-hidden="true"></span>
            }
            <img
                src={p.url.clone()}
                alt={t(lang, Key::image_alt)}
                loading="lazy"
                {onload}
                {onerror}
            />
        </span>
    }
}

/// The ledger gutter.
fn hash_slug(
    lang: Lang,
    m: &Message,
    chain: &BlockchainInfo,
    on_copy: Callback<MouseEvent>,
) -> Html {
    if m.msg_hash.is_empty() {
        return html! {};
    }
    match m.tx_hash.as_deref().filter(|t| !t.is_empty()) {
        Some(tx) => {
            let label = format!(
                "Message hash {}, published on {}",
                m.msg_hash,
                if chain.chain_name.is_empty() {
                    "chain"
                } else {
                    &chain.chain_name
                }
            );
            match chain.tx_url(tx) {
                Some(url) => html! {
                    <a class="fn-hash fn-hash--verified" href={url}
                       target="_blank" rel="noopener noreferrer"
                       title={label.clone()} aria-label={label}>
                        { m.hash_slug() }{ " " }{ t(lang, Key::verified_suffix) }
                    </a>
                },
                None => html! {
                    <span class="fn-hash fn-hash--verified" title={label.clone()} aria-label={label}>
                        { m.hash_slug() }{ " " }{ t(lang, Key::verified_suffix) }
                    </span>
                },
            }
        }
        None => html! {
            <button
                type="button"
                class="fn-hash"
                onclick={on_copy}
                title={t(lang, Key::message_hash_title).replace("{hash}", &m.msg_hash)}
                aria-label={t(lang, Key::copy_message_hash).replace("{hash}", &m.msg_hash)}
            >
                { m.hash_slug() }
            </button>
        },
    }
}

/// The reaction picker, anchored to this row like the `⋮` menu — the
/// `.fn-msg__tools > .fn-picker` rule in app.css does the placement,
/// including the flip-up near the bottom of the stream. A pick reacts and
/// closes.
fn reaction_picker(p: &MessageProps) -> Html {
    let on_pick = {
        let on_react = p.on_react.clone();
        let on_close = p.on_close_picker.clone();
        let id = p.message.id.clone();
        Callback::from(move |code: String| {
            on_react.emit((id.clone(), code, false));
            on_close.emit(());
        })
    };
    html! {
        <Picker open={p.picker_open} on_close={p.on_close_picker.clone()} {on_pick} />
    }
}

fn reaction_chip(
    id: &MessageId,
    code: &str,
    who: &[WalletAddress],
    me: &WalletAddress,
    on_react: &Callback<(MessageId, String, bool)>,
) -> Html {
    let mine = who.contains(me);
    let count = who.len();
    let label = if mine {
        format!("{code} {count} reactions, including you. Remove your reaction.")
    } else {
        format!("{code} {count} reactions. Add yours.")
    };
    let onclick = {
        let on_react = on_react.clone();
        let id = id.clone();
        let code = code.to_owned();
        Callback::from(move |_: MouseEvent| on_react.emit((id.clone(), code.clone(), mine)))
    };
    html! {
        <button
            type="button"
            class="fn-reaction"
            aria-pressed={mine.to_string()}
            aria-label={label.clone()}
            title={label}
            {onclick}
        >
            <span class="fn-reaction__emoji">{ code }</span>
            <span class="fn-nums">{ count }</span>
        </button>
    }
}

fn edit_box(
    lang: Lang,
    draft: &UseStateHandle<String>,
    commit: Callback<()>,
    editing: UseStateHandle<bool>,
) -> Html {
    let oninput = {
        let draft = draft.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                draft.set(el.value());
            }
        })
    };
    let onkeydown = {
        let commit = commit.clone();
        let editing = editing.clone();
        Callback::from(move |e: KeyboardEvent| match e.key().as_str() {
            "Enter" => {
                e.prevent_default();
                commit.emit(());
            }
            "Escape" => {
                e.stop_propagation();
                editing.set(false);
            }
            _ => {}
        })
    };
    html! {
        <div class="fn-row">
            <input
                class="topcoat-text-input fn-grow"
                type="text"
                aria-label={t(lang, Key::edit_message)}
                value={(**draft).clone()}
                {oninput}
                {onkeydown}
            />
            <button
                type="button"
                class="topcoat-icon-button"
                aria-label={t(lang, Key::save_edit)}
                onclick={{ let c = commit.clone(); Callback::from(move |_: MouseEvent| c.emit(())) }}
            >{ icons::check(16) }</button>
            <button
                type="button"
                class="topcoat-icon-button--quiet"
                aria-label={t(lang, Key::cancel_edit)}
                onclick={{ let e = editing.clone(); Callback::from(move |_: MouseEvent| e.set(false)) }}
            >{ icons::close(16) }</button>
        </div>
    }
}

fn menu(
    lang: Lang,
    p: &MessageProps,
    start_edit: Callback<MouseEvent>,
    is_open: bool,
    open: UseStateHandle<bool>,
) -> Html {
    let m = &p.message;
    let close = {
        let open = open.clone();
        move || open.set(false)
    };

    let copy_text = {
        let on_copy = p.on_copy.clone();
        let text = p.body.text().unwrap_or_default().to_owned();
        let close = close.clone();
        Callback::from(move |_: MouseEvent| {
            on_copy.emit(text.clone());
            close();
        })
    };
    let copy_hash = {
        let on_copy = p.on_copy.clone();
        let hash = m.msg_hash.clone();
        let close = close.clone();
        Callback::from(move |_: MouseEvent| {
            on_copy.emit(hash.clone());
            close();
        })
    };
    let copy_tx = {
        let on_copy = p.on_copy.clone();
        let tx = m.tx_hash.clone().unwrap_or_default();
        let close = close.clone();
        Callback::from(move |_: MouseEvent| {
            on_copy.emit(tx.clone());
            close();
        })
    };
    let delete = {
        let on_delete = p.on_delete.clone();
        let id = m.id.clone();
        let preview = p.body.text().map(str::to_owned);
        Callback::from(move |_: MouseEvent| {
            on_delete.emit((id.clone(), preview.clone()));
            open.set(false);
        })
    };

    html! {
        <Popover open={is_open} class="fn-picker" role="menu" label={t(lang, Key::message_actions)}
            on_dismiss={{ let close = close.clone(); Callback::from(move |_: ()| close()) }}>
            if p.body.text().is_some() {
                <button type="button" role="menuitem" class="topcoat-button--quiet" onclick={copy_text}>
                    { t(lang, Key::copy_text) }
                </button>
            }
            <button type="button" role="menuitem" class="topcoat-button--quiet" onclick={copy_hash}>
                { t(lang, Key::copy_hash) }
            </button>
            if m.tx_hash.as_deref().is_some_and(|t| !t.is_empty()) {
                <button type="button" role="menuitem" class="topcoat-button--quiet" onclick={copy_tx}>
                    { t(lang, Key::copy_tx_hash) }
                </button>
            }
            if p.body.text().is_some() {
                <button type="button" role="menuitem" class="topcoat-button--quiet" onclick={{
                    let on_knowledge = p.on_knowledge.clone();
                    let content = p.body.text().unwrap_or_default().to_owned();
                    let room_id = m.room_id.clone();
                    let id = m.id.clone();
                    let close = close.clone();
                    Callback::from(move |_: MouseEvent| {
                        on_knowledge.emit(crate::state::KnowledgeSeed::Teach {
                            content: content.clone(),
                            room_id: Some(room_id.clone()),
                            message_id: Some(id.clone()),
                        });
                        close();
                    })
                }}>
                    { t(lang, Key::teach_from_message) }
                </button>
            }
            if p.is_own && p.body.text().is_some() {
                <button type="button" role="menuitem" class="topcoat-button--quiet" onclick={start_edit}>
                    { t(lang, Key::edit) }
                </button>
            }
            // Any member may delete any message — "forgetting-first", and the
            // confirm dialog says so rather than hiding it.
            <button
                type="button"
                role="menuitem"
                class="topcoat-button--quiet fn-menuitem--danger"
                onclick={delete}
            >
                { t(lang, Key::delete) }
            </button>
        </Popover>
    }
}

#[cfg(test)]
mod tests {
    use super::{is_image_url, youtube_id};

    #[test]
    fn an_ai_generated_jpeg_url_is_an_image() {
        assert!(is_image_url(
            "https://imgen.x.ai/xai-imgen/xai-tmp-imgen-44149d59-d935-9750-b56b-eae69f693c87-e60b1f4f.jpeg"
        ));
    }

    #[test]
    fn a_query_string_does_not_hide_the_extension() {
        assert!(is_image_url("https://example.com/photo.PNG?w=800&fm=jpg"));
    }

    #[test]
    fn image_hosts_match_by_suffix_not_substring() {
        assert!(is_image_url("https://i.imgur.com/abc123"));
        assert!(!is_image_url("https://imgur.com.evil.example/abc123"));
    }

    #[test]
    fn an_ordinary_link_is_not_an_image() {
        assert!(!is_image_url("https://example.com/docs/readme"));
    }

    #[test]
    fn every_youtube_url_shape_yields_the_same_id() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?t=10&v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ?feature=share",
        ] {
            assert_eq!(youtube_id(url).as_deref(), Some("dQw4w9WgXcQ"), "{url}");
        }
    }

    #[test]
    fn a_malformed_video_id_is_not_embedded() {
        assert_eq!(youtube_id("https://youtu.be/short"), None);
        assert_eq!(
            youtube_id("https://notyoutube.com/watch?v=dQw4w9WgXcQ"),
            None
        );
    }
}

/// A day marker pill.
#[derive(Properties, PartialEq)]
pub struct DayMarkProps {
    pub timestamp: i64,
    pub now: i64,
    pub tz: i32,
}

#[function_component(DayMark)]
pub fn day_mark(p: &DayMarkProps) -> Html {
    html! { <div class="fn-daymark">{ format::day_marker(p.timestamp, p.now, p.tz) }</div> }
}
