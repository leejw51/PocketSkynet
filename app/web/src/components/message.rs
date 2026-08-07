//! One message row: bubble, ledger gutter, reactions and the hover tools
//! (DESIGN.md §7.2, §7.5).
//!
//! The **ledger gutter** is the element this product is remembered by: an
//! 8-character `msgHash` slug in mono under every bubble, at 55 % opacity until
//! hover. When the hash has been anchored on-chain it turns emerald and gains a
//! check. No other messenger has a receipt stub, and it is the visible reason
//! the protocol hashes every message.

use std::rc::Rc;

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
    /// How many replies this message has, 0 for none. Drives the footer that
    /// opens the thread.
    #[prop_or_default]
    pub reply_count: i64,
    /// This message's thread is expanded below it.
    #[prop_or_default]
    pub thread_open: bool,
    /// Open or close this message's thread.
    #[prop_or_default]
    pub on_toggle_thread: Callback<MessageId>,
    /// Start composing a reply into this message's thread.
    #[prop_or_default]
    pub on_reply: Callback<MessageId>,
    /// Rendered *inside* a thread rather than in the channel. Suppresses the
    /// reply affordances, because a reply already has a thread — replying to
    /// one joins the same thread, so a second "reply" control on a reply
    /// would promise a nesting that does not exist.
    #[prop_or_default]
    pub in_thread: bool,
    /// Display names of everybody in this room, for highlighting `@mentions`.
    ///
    /// Names rather than the roster, and shared rather than cloned: this is
    /// the same list on every row of a long conversation, and copying a
    /// hundred `RoomMember`s per render to draw a chip would be the most
    /// expensive thing on the screen.
    #[prop_or_default]
    pub mention_names: Rc<Vec<String>>,
    /// The handles that mean *the viewer* — their display name and their
    /// address. A mention of the reader is drawn differently, and answering
    /// "is this one mine" needs the viewer's *name*, which their address alone
    /// does not give.
    #[prop_or_default]
    pub my_handles: Rc<Vec<String>>,
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

    // The link this message carries, as an address another device can open.
    // Computed here because it needs the store: the host to name is the
    // server's recommendation, not this page's origin.
    let share_link = p
        .body
        .text()
        .and_then(link_in)
        .map(|url| store.shareable_url(url));

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
                        subtitle: None,
                        address: Some(m.sender_address.clone()),
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

            // The thread footer. Only on a message that has replies, and never
            // inside a thread — where every row is already in one.
            if p.reply_count > 0 && !p.in_thread {
                <button
                    type="button"
                    class={classes!("fn-thread-open", p.thread_open.then_some("is-open"))}
                    aria-expanded={p.thread_open.to_string()}
                    onclick={{
                        let on_toggle_thread = p.on_toggle_thread.clone();
                        let id = m.id.clone();
                        Callback::from(move |_: MouseEvent| on_toggle_thread.emit(id.clone()))
                    }}
                >
                    { icons::thread(14) }
                    <span>{ t(lang, if p.reply_count == 1 {
                                Key::thread_reply_one
                            } else {
                                Key::thread_reply_many
                            }).replace("{n}", &p.reply_count.to_string()) }</span>
                </button>
            }

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
                if !p.in_thread {
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::reply_in_thread)}
                        title={t(lang, Key::reply_in_thread)}
                        onclick={{
                            let on_reply = p.on_reply.clone();
                            let id = m.id.clone();
                            Callback::from(move |_: MouseEvent| on_reply.emit(id.clone()))
                        }}
                    >
                        { icons::thread(16) }
                    </button>
                }
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
                { menu(lang, p, share_link.clone(), start_edit, *menu_open, menu_open.clone()) }
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
            <div class="fn-bubble">
                { render_with_mentions(lang, text, &p.mention_names, &p.my_handles, &on_tag) }
            </div>
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
/// Draw the body, with any `@name` in it as a chip.
///
/// Mentions are found by *span* rather than by token because a username may
/// contain spaces (`validate::username` allows them), so "@Jonghwan Lee" is one
/// mention and two tokens. The text between spans goes through
/// [`render_content`] unchanged, which is what keeps links, hashtags,
/// attachments and addresses working inside a sentence that also names
/// somebody.
fn render_with_mentions(
    lang: Lang,
    text: &str,
    names: &[String],
    my_handles: &[String],
    on_tag: &Callback<String>,
) -> Html {
    let spans = crate::mentions::highlight_spans(text, names);
    if spans.is_empty() {
        return render_content(lang, text, on_tag);
    }

    // A mention *of the viewer* is drawn differently. Being named is the one
    // thing in a busy room somebody is scanning for, and a chip that looks the
    // same whoever it names does not answer that question.
    let mut out: Vec<Html> = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start > cursor {
            out.push(render_content(lang, &text[cursor..start], on_tag));
        }
        let label = &text[start..end];
        let is_me = my_handles
            .iter()
            .any(|h| label[1..].eq_ignore_ascii_case(h));
        out.push(html! {
            <span class={classes!("fn-mention", is_me.then_some("fn-mention--me"))}>
                { label }
            </span>
        });
        cursor = end;
    }
    if cursor < text.len() {
        out.push(render_content(lang, &text[cursor..], on_tag));
    }
    html! { <>{ for out }</> }
}

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
        } else if let Some(media) = hosted_media_in(token) {
            // Media this server hosts, posted as the same-origin path it is
            // served at — every AI generation lands here. Before this it
            // rendered as the bare text `/api/images/<64 hex>.png`, which is
            // the one shape in a room that is unambiguously a picture and
            // was the only one shown as a string.
            //
            // The thumbnail route hangs off the media URL itself, so it works
            // whether the token was written relative or absolute. Media hosted
            // before thumbnails existed 404s there; both embeds treat that as
            // "use the original", which was the old behaviour.
            let thumb = format!("{}/thumbnail", media.url);
            out.push(match media.kind {
                MediaKind::Image => {
                    html! { <ImageEmbed url={media.url.to_owned()} thumb={thumb} /> }
                }
                MediaKind::Video => {
                    html! { <VideoEmbed url={media.url.to_owned()} poster={thumb} /> }
                }
            });
        } else if token.starts_with("https://") || token.starts_with("http://") {
            if let Some(id) = youtube_id(token) {
                out.push(youtube_embed(lang, &id));
            } else if is_image_url(token) {
                out.push(html! { <ImageEmbed url={token.to_owned()} /> });
            } else if is_video_url(token) {
                out.push(html! { <VideoEmbed url={token.to_owned()} /> });
            } else {
                out.push(html! {
                    <a href={token.to_owned()} target="_blank" rel="noopener noreferrer">{ token }</a>
                });
            }
        } else if let Some((addr, tail)) = wallet_in(token) {
            // A pasted wallet address is the one piece of message content
            // people reliably want *back out* of the conversation, and it is
            // also the one piece nobody can select accurately with a thumb.
            // Tapping it copies the checksummed form.
            //
            // Shown abbreviated, with the eye beside it. This used to render
            // full-length on the argument that the text should still say what
            // was typed — but a bubble is not a transcript, it is a reading
            // surface, and forty-two mono characters mid-sentence break the
            // line they are in and dominate every message around them. What
            // was typed is one tap away and on the clipboard either way.
            out.push(html! { <Addr address={addr} revealable=true /> });
            if !tail.is_empty() {
                out.push(html! { { tail.to_owned() } });
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

/// A bare wallet address written into a message, plus whatever punctuation was
/// stuck to the end of it.
///
/// The tail is returned rather than swallowed: "pay 0xabc…123." ends in a full
/// stop that belongs to the sentence, not to the address, and a renderer that
/// eats it is quietly rewriting what somebody said.
fn wallet_in(token: &str) -> Option<(WalletAddress, &str)> {
    const TRAILING: [char; 10] = ['.', ',', ';', ':', '!', '?', ')', ']', '"', '\''];
    let head = token.trim_end_matches(TRAILING);
    let addr = WalletAddress::new(head).ok()?;
    Some((addr, &token[head.len()..]))
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

/// Video by file extension, for a URL that is a video file rather than a
/// page about one. Deliberately narrow: only what a `<video>` element can
/// actually play without a codec surprise.
fn is_video_url(url: &str) -> bool {
    const EXTENSIONS: [&str; 3] = [".mp4", ".webm", ".ogv"];
    let Some((_, path, _)) = split_url(url) else {
        return false;
    };
    let path = path.to_ascii_lowercase();
    EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// The first link in a message worth copying: media this server hosts, or any
/// `http(s)` URL somebody typed.
///
/// Returned relative when that is how it was written — [`AppState::shareable_url`]
/// is what turns it into an address for another device, and it is the only
/// place that decides which host to name.
///
/// [`AppState::shareable_url`]: crate::state::AppState::shareable_url
fn link_in(text: &str) -> Option<&str> {
    text.split_whitespace().find(|token| {
        hosted_media_in(token).is_some()
            || token.starts_with("https://")
            || token.starts_with("http://")
    })
}

/// What an embed should be drawn as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MediaKind {
    Image,
    Video,
}

/// Media hosted by a PocketSkynet server, found in a message.
///
/// The AI assistant posts its generations as the path they are served at —
/// `/api/images/<sha256>.<ext>` — because a relative URL keeps working when
/// the same server is reached over Tailscale, over the LAN, and over
/// loopback, which one baked-in absolute URL does not.
struct HostedMedia<'a> {
    url: &'a str,
    kind: MediaKind,
}

/// Recognise a hosted-media URL, relative or absolute.
///
/// The shape is checked, not just the prefix: exactly a 64-character hex
/// digest and one known extension, which is the same rule the server applies
/// before it will serve the file. Anything else is left as text rather than
/// turned into an element pointing at a path that cannot resolve.
fn hosted_media_in(token: &str) -> Option<HostedMedia<'_>> {
    const PREFIX: &str = "/api/images/";
    let name = match token.strip_prefix(PREFIX) {
        Some(rest) => rest.to_owned(),
        // The absolute form: what the assistant's copy button hands over, so
        // pasting that back into a room has to render as the picture too.
        None => split_url(token)?.1.strip_prefix(PREFIX)?.to_owned(),
    };
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.len() != 64 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let kind = match ext.to_ascii_lowercase().as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" => MediaKind::Image,
        "mp4" | "webm" => MediaKind::Video,
        _ => return None,
    };
    Some(HostedMedia { url: token, kind })
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
    /// The attachment is not there *yet*.
    ///
    /// A distinct state from `Failed` because the two mean opposite things to
    /// somebody staring at a video they just posted. Finishing an upload
    /// rehashes the whole file server-side before the `files` row exists
    /// (`routes/uploads.rs`), and for a large video that pass takes real time
    /// — during which the attachment genuinely 404s. Reporting that as "could
    /// not be loaded" tells the viewer their upload broke, at exactly the
    /// moment it is being checked.
    Verifying,
    Loaded,
    Failed,
}

#[derive(Properties, PartialEq)]
struct AttachmentEmbedProps {
    id: AttrValue,
}

/// An attachment inside a message.
///
/// A preview is a **URL**, not a buffer.
///
/// It used to be the other way round: `/api/files/{id}/raw` needs a bearer
/// token, an `<img src>` cannot send one, so the bytes were fetched and wrapped
/// in an object URL — which meant a preview cost the whole file, and a film
/// could not be previewed at all, only downloaded and then played.
///
/// Now the component asks for a short-lived single-file capability and points
/// the element straight at it with `?inline=1`. The server streams and honours
/// `Range`, so a `<video>` fetches what it is playing and seeks by asking for
/// the byte range it lands on. A two-hour film costs the page nothing, there is
/// no object URL to revoke, and the size cap that used to guard the buffer is
/// gone with the buffer.
#[function_component(AttachmentEmbed)]
fn attachment_embed(p: &AttachmentEmbedProps) -> Html {
    let store = crate::state::use_store();
    let lang = store.language;
    let meta = use_state(|| Option::<crate::api::FileMeta>::None);
    let blob = use_state(|| Option::<String>::None);
    // The thumbnail URL, when the server holds one — carrying the same
    // capability the full URL does, so it costs no extra mint.
    let thumb = use_state(|| Option::<String>::None);
    let load = use_state(|| MediaLoad::Loading);
    // False until the viewer asks for the film. See the render below for why a
    // video is a still first.
    let playing = use_state(|| false);
    let img = use_node_ref();

    {
        let store = store.clone();
        let meta = meta.clone();
        let blob = blob.clone();
        let thumb = thumb.clone();
        let load = load.clone();
        let id = p.id.to_string();
        use_effect_with(id.clone(), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                // Retry a miss rather than failing on it. The window is the
                // time a server needs to hash a large file, and each attempt
                // waits longer than the last so a small file still resolves
                // in one go and a large one is not polled hard.
                const ATTEMPTS: u32 = 8;
                let mut file = None;
                for attempt in 0..ATTEMPTS {
                    match store.client.file(&id).await {
                        Ok(f) => {
                            file = Some(f);
                            break;
                        }
                        Err(e) if e.is_not_found() && attempt + 1 < ATTEMPTS => {
                            // Only a 404 is worth waiting through: it is the
                            // shape "the row is not written yet" takes. A 403
                            // or a network error will not improve by asking
                            // again, and pretending otherwise would leave a
                            // spinner up for fifteen seconds before saying so.
                            load.set(MediaLoad::Verifying);
                            gloo_timers::future::TimeoutFuture::new(400 << attempt.min(5)).await;
                        }
                        Err(_) => break,
                    }
                }
                let Some(file) = file else {
                    load.set(MediaLoad::Failed);
                    return;
                };
                // No size test: streaming costs the page nothing, so a 4 GB
                // film is as previewable as a thumbnail.
                let previewable = file.is_previewable_image() || file.is_previewable_video();
                meta.set(Some(file));
                if !previewable {
                    // Nothing to point at: the card is the whole render.
                    load.set(MediaLoad::Loaded);
                    return;
                }
                match store.client.download_link(&id).await {
                    Ok(link) => {
                        // `inline=1` asks the server for a real media
                        // Content-Type; without it the element is handed
                        // octet-stream and plays nothing.
                        blob.set(Some(store.client.url(&format!("{}&inline=1", link.url))));
                        thumb.set(link.thumb_url.map(|u| store.client.url(&u)));
                        load.set(MediaLoad::Loaded);
                    }
                    Err(_) => load.set(MediaLoad::Failed),
                }
            });
            || ()
        });
    }

    // No unmount cleanup any more, and its absence is the point. This used to
    // revoke an object URL, because without that a room with twenty videos
    // pinned twenty copies of them for the life of the document. There is
    // nothing to pin now — the element holds a URL, and the browser drops
    // whatever it had buffered when the element goes.

    if *load == MediaLoad::Failed {
        return html! {
            <span class="fn-media--failed">{ t(lang, Key::attachment_failed) }</span>
        };
    }
    if *load == MediaLoad::Verifying {
        return html! {
            <span class="fn-attach fn-attach--loading">
                <span class="fn-spinner" aria-hidden="true"></span>
                <span>{ t(lang, Key::attachment_verifying) }</span>
            </span>
        };
    }
    let Some(file) = (*meta).clone() else {
        return html! {
            <span class="fn-attach fn-attach--loading">
                <span class="fn-spinner" aria-hidden="true"></span>
            </span>
        };
    };

    // Tapping the picture zooms it in place. It used to open a new window,
    // which is the heavier of the two gestures pointing at the same wish — to
    // see the thing bigger — and the one that costs a tab. The new window is
    // still on the toolbar for anyone who wanted the window itself.
    //
    // When the bubble is showing the *thumbnail*, the lightbox must not: the
    // whole point of the zoom is to read the full picture, so the shot names
    // the full URL while the entrance still travels from the thumbnail's rect.
    let open_zoom = {
        let img = img.clone();
        let filename = file.filename.clone();
        let full = thumb.is_some().then(|| (*blob).clone()).flatten();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            zoom_past_thumbnail(&img, full.clone(), Some(filename.clone()));
        })
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
                // Streams via a capability URL rather than pulling the bytes
                // through the page — see `actions::save_attachment`.
                crate::actions::save_attachment(store, file.id.clone(), file.filename.clone())
                    .await;
            });
        })
    };

    let media = match (&*blob, file.is_previewable_video()) {
        // A video in the stream is a **still**, not a player, until it is
        // asked for. The still is the server-held poster when there is one —
        // a few tens of kilobytes — and only falls back to
        // `preload="metadata"` (the header and one frame of a film that may
        // be gigabytes) for videos uploaded before posters existed. A room
        // full of videos now costs a room full of *thumbnails*, in the small
        // sense of the word this comment always wanted.
        //
        // Clicking swaps in the real player: `controls`, `autoplay`, and
        // `preload="auto"`, which is the point at which the browser actually
        // starts pulling the film down. Nothing plays until then, so scrolling
        // past a video never makes noise and never spends anyone's bandwidth.
        (Some(url), true) if !*playing => html! {
            <button
                type="button"
                class="fn-attach__shot fn-attach__play"
                aria-label={t(lang, Key::video_play)}
                title={t(lang, Key::video_play)}
                onclick={{
                    let playing = playing.clone();
                    Callback::from(move |_: MouseEvent| playing.set(true))
                }}
            >
                if let Some(poster) = (*thumb).clone() {
                    <img
                        class="fn-attach__media"
                        src={poster}
                        alt={file.filename.clone()}
                        loading="lazy"
                    />
                } else {
                    <video
                        class="fn-attach__media"
                        src={url.clone()}
                        muted=true
                        preload="metadata"
                    />
                }
                <span class="fn-attach__play-badge" aria-hidden="true">
                    { icons::play(22) }
                </span>
            </button>
        },
        (Some(url), true) => html! {
            <video
                class="fn-attach__media"
                src={url.clone()}
                controls=true
                autoplay=true
                preload="auto"
            />
        },
        // A picture renders from its thumbnail when the server holds one —
        // the bubble is capped at 400px, so full bytes here were always
        // decoration — and the tap zooms to the full URL (see `open_zoom`).
        (Some(url), false) => html! {
            <button
                type="button"
                class="fn-attach__shot"
                aria-label={t(lang, Key::image_zoom)}
                title={t(lang, Key::image_zoom)}
                onclick={open_zoom}
            >
                <img
                    ref={img}
                    class="fn-attach__media"
                    src={(*thumb).clone().unwrap_or_else(|| url.clone())}
                    alt={file.filename.clone()}
                />
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

/// Raise the lightbox for the **full** picture from an `<img>` that may be
/// showing only its thumbnail.
///
/// With no `full` URL the element's own src is already the picture and
/// [`lightbox::zoom`] does everything. With one, the shot names the full URL
/// while the entrance still travels from the thumbnail's painted rect — same
/// place, same shape (a thumbnail shares its original's aspect ratio), sharper
/// pixels arriving as they load.
///
/// [`lightbox::zoom`]: super::lightbox::zoom
pub(super) fn zoom_past_thumbnail(node: &NodeRef, full: Option<String>, caption: Option<String>) {
    let Some(src) = full else {
        super::lightbox::zoom(node, caption);
        return;
    };
    let Some(img) = node.cast::<web_sys::HtmlImageElement>() else {
        return;
    };
    let natural = (
        f64::from(img.natural_width()),
        f64::from(img.natural_height()),
    );
    let r = img.get_bounding_client_rect();
    let area = super::lightbox::Rect {
        x: r.left(),
        y: r.top(),
        w: r.width(),
        h: r.height(),
    };
    super::lightbox::show(super::lightbox::Shot {
        src,
        alt: img.alt(),
        caption,
        origin: Some(super::lightbox::painted(area, natural.0, natural.1)),
        // The full picture's pixel size is unknown until it loads; the
        // lightbox borrows the thumbnail's shape, which is the same shape.
        natural: None,
    });
}

#[derive(Properties, PartialEq)]
struct ImageEmbedProps {
    url: AttrValue,
    /// A smaller copy to show in the bubble, when the server holds one.
    /// The zoom always names the full `url`.
    #[prop_or_default]
    thumb: Option<AttrValue>,
}

/// An inline image: lazy, a spinner where the image will land, and a failure
/// row that keeps the URL clickable (DESIGN.md §7.2 "Content").
///
/// Tapping it raises the lightbox. The picture is capped at 400px in the
/// bubble — which is the right size for a conversation and the wrong size for
/// reading a screenshot — so the zoom is not a flourish, it is the only way to
/// actually see what somebody posted.
#[function_component(ImageEmbed)]
fn image_embed(p: &ImageEmbedProps) -> Html {
    let lang = crate::state::use_store().language;
    let load = use_state(|| MediaLoad::Loading);
    // The thumbnail is an optimisation, so its failure must not be one: media
    // hosted before thumbnails existed 404s there, and the answer is the full
    // picture — the pre-thumbnail behaviour — not a broken row.
    let thumb_failed = use_state(|| false);
    let img = use_node_ref();

    if *load == MediaLoad::Failed {
        return html! {
            <span class="fn-media--failed">
                { t(lang, Key::image_failed) }
                { " " }
                <a href={p.url.clone()} target="_blank" rel="noopener noreferrer">{ &p.url }</a>
            </span>
        };
    }

    let showing_thumb = p.thumb.is_some() && !*thumb_failed;
    let src: AttrValue = if showing_thumb {
        p.thumb.clone().unwrap_or_else(|| p.url.clone())
    } else {
        p.url.clone()
    };

    let onload = {
        let load = load.clone();
        Callback::from(move |_: Event| load.set(MediaLoad::Loaded))
    };
    let onerror = {
        let load = load.clone();
        let thumb_failed = thumb_failed.clone();
        let falls_back = showing_thumb;
        Callback::from(move |_: Event| {
            if falls_back {
                thumb_failed.set(true);
            } else {
                load.set(MediaLoad::Failed);
            }
        })
    };
    // A button, not a bare `onclick` on the image: this is a control, and a
    // control that only a mouse can reach is not one. The whole picture is the
    // hit area, because on a phone that is the only hit area there is.
    let zoom = {
        let img = img.clone();
        let full = showing_thumb.then(|| p.url.to_string());
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            zoom_past_thumbnail(&img, full.clone(), None);
        })
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
            <button
                type="button"
                class="fn-media__zoom"
                aria-label={t(lang, Key::image_zoom)}
                title={t(lang, Key::image_zoom)}
                onclick={zoom}
            >
                <img
                    ref={img}
                    {src}
                    alt={t(lang, Key::image_alt)}
                    loading="lazy"
                    {onload}
                    {onerror}
                />
            </button>
        </span>
    }
}

#[derive(Properties, PartialEq)]
struct VideoEmbedProps {
    url: AttrValue,
    /// A poster frame, when the server holds one. A 404 here is harmless —
    /// the element falls back to the metadata frame it fetches anyway.
    #[prop_or_default]
    poster: Option<AttrValue>,
}

/// An inline video: the clip itself, with controls, where the URL would have
/// been.
///
/// `preload="metadata"` rather than `auto`: a room can hold a dozen of these
/// and a scroll past them must not pull down a dozen whole files. Muted and
/// never autoplaying — a message that starts making noise when it arrives is
/// a message nobody wants twice. On failure the URL stays clickable, exactly
/// as `ImageEmbed` does.
#[function_component(VideoEmbed)]
fn video_embed(p: &VideoEmbedProps) -> Html {
    let lang = crate::state::use_store().language;
    let load = use_state(|| MediaLoad::Loading);

    if *load == MediaLoad::Failed {
        return html! {
            <span class="fn-media--failed">
                { t(lang, Key::video_failed) }
                { " " }
                <a href={p.url.clone()} target="_blank" rel="noopener noreferrer">{ &p.url }</a>
            </span>
        };
    }

    let onloadeddata = {
        let load = load.clone();
        Callback::from(move |_: Event| load.set(MediaLoad::Loaded))
    };
    let onerror = {
        let load = load.clone();
        Callback::from(move |_: Event| load.set(MediaLoad::Failed))
    };
    let mut class = classes!("fn-media", "fn-media--clip");
    if *load == MediaLoad::Loading {
        class.push("fn-media--loading");
    }
    html! {
        <span {class}>
            if *load == MediaLoad::Loading {
                <span class="fn-spinner" aria-hidden="true"></span>
            }
            <video
                src={p.url.clone()}
                poster={p.poster.clone()}
                controls=true
                playsinline=true
                preload="metadata"
                aria-label={t(lang, Key::video_alt)}
                onloadeddata={onloadeddata}
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

/// The `⋮` menu. `share_link` is the message's link as an absolute URL for
/// another device, when it has one — an AI generation this server hosts, or a
/// pasted `http(s)` URL.
fn menu(
    lang: Lang,
    p: &MessageProps,
    share_link: Option<String>,
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
    // Copying the *text* of a message whose text is a URL gives a relative
    // path that only resolves inside this app. This gives the address to send
    // somebody — which for a picture generated here is the whole point of
    // having generated it.
    let copy_link = {
        let on_copy = p.on_copy.clone();
        let link = share_link.clone().unwrap_or_default();
        let close = close.clone();
        Callback::from(move |_: MouseEvent| {
            on_copy.emit(link.clone());
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
            // First, when there is one: for a picture or a clip this is the
            // only entry anybody wants, and it sits above "Copy text" because
            // "Copy text" on such a message hands over a path with no host —
            // a link that resolves nowhere but here.
            if share_link.is_some() {
                <button type="button" role="menuitem" class="topcoat-button--quiet" onclick={copy_link}>
                    { t(lang, Key::copy_link) }
                </button>
            }
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
    use super::{hosted_media_in, is_image_url, is_video_url, wallet_in, youtube_id, MediaKind};

    /// A 64-character hex digest, the only stem this server serves.
    fn digest() -> String {
        "41bd9a30a292e7860bc9ddbae8c5c5c1907f972cc657665cdf6baee6e61a2008".to_owned()
    }

    #[test]
    fn an_ai_generation_posted_into_a_room_renders_as_the_media_not_the_path() {
        // The exact shape the assistant posts. This used to be shown as a
        // line of text, which is the bug this recognises.
        let png = format!("/api/images/{}.png", digest());
        assert_eq!(hosted_media_in(&png).unwrap().kind, MediaKind::Image);

        let mp4 = format!("/api/images/{}.mp4", digest());
        let found = hosted_media_in(&mp4).unwrap();
        assert_eq!(found.kind, MediaKind::Video);
        // The URL is passed through untouched: what was written is what is
        // loaded, and a relative path stays relative.
        assert_eq!(found.url, mp4);

        for ext in ["jpg", "jpeg", "webp", "gif", "webm"] {
            let url = format!("/api/images/{}.{ext}", digest());
            assert!(hosted_media_in(&url).is_some(), "{ext}");
        }
    }

    #[test]
    fn the_absolute_form_from_the_copy_button_renders_too() {
        let url = format!("https://home.example:8443/api/images/{}.png", digest());
        assert_eq!(hosted_media_in(&url).unwrap().kind, MediaKind::Image);
        assert_eq!(hosted_media_in(&url).unwrap().url, url);
    }

    #[test]
    fn only_a_real_hosted_name_becomes_an_element() {
        for text in [
            // Right prefix, wrong shape — an element pointing at this would
            // just 404, so it stays text.
            "/api/images/notahash.png",
            &format!("/api/images/{}.png", "a".repeat(63)),
            &format!("/api/images/{}.exe", digest()),
            &format!("/api/images/{}", digest()),
            "/api/images/",
            // Traversal shapes never reach an `<img>`.
            "/api/images/../jwt.secret.png",
            // A different route that merely looks similar.
            &format!("/api/files/{}/raw", digest()),
            "just some text",
        ] {
            assert!(hosted_media_in(text).is_none(), "{text}");
        }
    }

    #[test]
    fn copy_link_finds_the_link_a_message_actually_carries() {
        let png = format!("/api/images/{}.png", digest());
        assert_eq!(super::link_in(&png), Some(png.as_str()));
        // Prose around it, and a newline rather than a space, still finds it.
        let with_words = format!("look at this\n{png} nice?");
        assert_eq!(super::link_in(&with_words), Some(png.as_str()));

        // An ordinary pasted URL is copyable too.
        assert_eq!(
            super::link_in("see https://example.com/a"),
            Some("https://example.com/a")
        );

        // Nothing to copy: the menu entry must not appear.
        assert_eq!(super::link_in("just a sentence"), None);
        assert_eq!(super::link_in("/api/images/notahash.png"), None);
        assert_eq!(super::link_in(""), None);
    }

    #[test]
    fn a_video_file_url_is_a_clip_and_a_page_about_one_is_not() {
        assert!(is_video_url("https://vidgen.x.ai/xai-video/abc.mp4"));
        assert!(is_video_url("https://example.com/clip.WEBM?t=3"));
        assert!(!is_video_url("https://example.com/watch/clip"));
        // Images and videos must not both claim the same URL.
        assert!(!is_image_url("https://example.com/clip.mp4"));
        assert!(!is_video_url("https://example.com/photo.png"));
    }

    #[test]
    fn a_pasted_address_is_recognised_with_its_punctuation_intact() {
        let (addr, tail) = wallet_in("0x742d35Cc6634C0532925a3b844Bc454e4438f44e.").unwrap();
        assert_eq!(addr.as_str(), "0x742d35cc6634c0532925a3b844bc454e4438f44e");
        assert_eq!(tail, ".");
    }

    #[test]
    fn a_hex_string_of_the_wrong_length_is_just_text() {
        assert!(wallet_in("0xdeadbeef").is_none());
        assert!(wallet_in("742d35Cc6634C0532925a3b844Bc454e4438f44e").is_none());
    }

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
