//! Shared primitives: the identity tile, addresses, badges, spinners, empty and
//! error states, banners, skeletons and the connection pill.
//!
//! Every one of these maps 1:1 onto a class in `app.css` §4 and §11. Nothing
//! here invents styling; the components exist so the class strings, the ARIA
//! attributes and the accessible names are written once instead of at each of
//! the two hundred call sites that would otherwise get one of them wrong.

use pocketskynet_core::WalletAddress;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsValue;
use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::identity;
use crate::realtime::ConnStatus;

use super::icons;

/// Identity tile sizes (DESIGN.md §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentSize {
    /// 24px — inline, pick lists.
    Xs,
    /// 28px — compact rows.
    Sm,
    /// 36px — room rows, message gutter.
    #[default]
    Md,
    /// 44px — member rows.
    Lg,
    /// 64px — the profile card.
    Xl,
}

impl IdentSize {
    fn class(self) -> &'static str {
        match self {
            IdentSize::Xs => " fn-ident--xs",
            IdentSize::Sm => " fn-ident--sm",
            IdentSize::Md => "",
            IdentSize::Lg => " fn-ident--lg",
            IdentSize::Xl => " fn-ident--xl",
        }
    }
}

/// What the spotlight should say when a tile is tapped to zoom it.
///
/// Carried by the caller rather than derived here, because only the caller
/// knows the display name and whether the address is worth a copy button.
#[derive(Debug, Clone, PartialEq)]
pub struct Zoom {
    /// The name under the portrait — also the tile's accessible label.
    pub title: String,
    /// A quieter second line, usually the checksummed address.
    pub subtitle: Option<String>,
    /// When set, the stage offers a copy button for this value.
    pub copy: Option<String>,
}

#[derive(Properties, PartialEq)]
pub struct IdentProps {
    /// A wallet address for a person, or a room id for a room. **Never mix the
    /// two seed spaces** — a room that borrowed its creator's chip would be
    /// unrecognisable the moment ownership changed.
    pub seed: String,
    #[prop_or_default]
    pub size: IdentSize,
    /// Adds the orange ring that means "this is you" — the only avatar
    /// treatment in the product that carries meaning.
    #[prop_or_default]
    pub is_self: bool,
    /// The presence dot. Only pass `true` with *real* presence data; the React
    /// client shows it unconditionally, which is a lie.
    #[prop_or_default]
    pub online: bool,
    /// A single letter drawn in the corner, used for room chips.
    #[prop_or_default]
    pub corner: Option<String>,
    /// The person's *chosen* avatar (`User.profileImage`), when the caller
    /// has a profile at hand. Resolved through [`identity::avatar_src`], so
    /// an unknown or hostile value silently falls back to the hash tile.
    #[prop_or_default]
    pub image: Option<String>,
    /// When set, tapping the tile raises the spotlight — the full-screen
    /// zoom of this exact portrait — so a face can be checked at more than
    /// forty pixels. The tile becomes a real button (focusable, labelled)
    /// and stops being `aria-hidden` decoration.
    #[prop_or_default]
    pub zoom: Option<Zoom>,
    #[prop_or_default]
    pub class: Classes,
}

/// The signature element: a flat, deterministic colour tile carrying two
/// characters of the identifier it stands for.
///
/// Always `aria-hidden`. It is redundant decoration — the name and the address
/// are always beside it — and an avatar that reads its own colour out to a
/// screen reader is noise.
#[function_component(Ident)]
pub fn ident_tile(p: &IdentProps) -> Html {
    let monogram = identity::monogram_for(&p.seed);
    let hue = identity::hue_for(&p.seed);
    // A chosen avatar wins over the hash-derived face; the coloured tile and
    // monogram stay underneath either way, as the loading/failure fallback.
    let src = p
        .image
        .as_deref()
        .and_then(identity::avatar_src)
        .unwrap_or_else(|| format!("/static/img/{}.png", identity::art_for(&p.seed)));
    let mut class = classes!("fn-ident", "fn-ident--art", p.class.clone());
    class.push(p.size.class().trim());
    if p.is_self {
        class.push("fn-ident--self");
    }
    if p.online {
        class.push("fn-ident--online");
    }
    if p.zoom.is_some() {
        class.push("fn-ident--zoom");
    }

    // One closure for the click and the keyboard, so the two can never
    // drift. Propagation stops here: several tiles sit inside rows that have
    // their own click (a room row opens the room), and a zoom that *also*
    // navigated would be two answers to one tap.
    let fire = p.zoom.clone().map(|zoom| {
        let seed = p.seed.clone();
        let image = p.image.clone();
        std::rc::Rc::new(move || {
            super::spotlight::show_identity(
                &seed,
                image.as_deref(),
                zoom.title.clone(),
                zoom.subtitle.clone(),
                zoom.copy.clone(),
            );
        })
    });
    let onclick = fire.clone().map(|fire| {
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            fire();
        })
    });
    let onkeydown = fire.map(|fire| {
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" || e.key() == " " {
                e.prevent_default();
                e.stop_propagation();
                fire();
            }
        })
    });

    html! {
        <span
            {class}
            style={format!("--fn-hue: {hue}")}
            // Decoration by default; a real, labelled control when zoomable.
            aria-hidden={p.zoom.is_none().then_some("true")}
            role={p.zoom.is_some().then_some("button")}
            tabindex={p.zoom.is_some().then_some("0")}
            aria-label={p.zoom.as_ref().map(|z| z.title.clone())}
            {onclick}
            {onkeydown}
        >
            // The coloured tile stays underneath as the loading and fallback
            // state: 110KB arrives over a network, and a blank square where a
            // face should be is worse than the letters that were there before.
            // `decoding="async"` keeps eighteen of these off the main thread
            // when a room list paints them all at once.
            <img
                class="fn-ident__art"
                {src}
                alt=""
                loading="lazy"
                decoding="async"
            />
            <span class="fn-ident__mono">{ monogram }</span>
            if let Some(c) = &p.corner {
                <b class="fn-ident__corner">{ c }</b>
            }
        </span>
    }
}

#[derive(Properties, PartialEq)]
pub struct AddrProps {
    pub address: WalletAddress,
    /// Show the full EIP-55 checksum rather than `0x9f2a…7c41`.
    #[prop_or_default]
    pub full: bool,
}

/// A wallet address in mono.
///
/// The `aria-label` always carries the **full checksum** even when the visible
/// text is truncated, so a screen-reader user can read out the whole address
/// (DESIGN.md §17). EIP-55 casing is display-only; comparisons elsewhere use
/// the lowercase form the newtype guarantees.
#[function_component(Addr)]
pub fn addr(p: &AddrProps) -> Html {
    let lang = crate::state::use_store().language;
    let checksum = p.address.to_checksummed();
    let text = if p.full {
        checksum.clone()
    } else {
        p.address.abbreviated()
    };
    let class = if p.full {
        "fn-addr fn-addr--full"
    } else {
        "fn-addr"
    };
    html! { <span {class} aria-label={t(lang, Key::wallet_address_aria).replace("{address}", &checksum)}>{ text }</span> }
}

#[derive(Properties, PartialEq)]
pub struct LockProps {
    /// Recoloured to the warning yellow while a rotation is pending, because
    /// "encrypted" and "encrypted but you can't post" are different states.
    #[prop_or_default]
    pub pending: bool,
}

/// The emerald padlock that means *encryption is working*.
#[function_component(Lock)]
pub fn lock(p: &LockProps) -> Html {
    let (class, label) = if p.pending {
        ("fn-lock fn-lock--pending", "Encrypted, key rotation needed")
    } else {
        ("fn-lock", "Encrypted")
    };
    html! { <span {class} role="img" aria-label={label} title={label}></span> }
}

#[derive(Properties, PartialEq)]
pub struct BadgeProps {
    #[prop_or_default]
    pub variant: &'static str,
    pub children: Children,
}

/// A small status pill. `variant` is one of `admin`, `self`, `muted`, `danger`,
/// `info`, `encrypt`.
#[function_component(Badge)]
pub fn badge(p: &BadgeProps) -> Html {
    let class = if p.variant.is_empty() {
        "fn-badge".to_owned()
    } else {
        format!("fn-badge fn-badge--{}", p.variant)
    };
    html! { <span {class}>{ for p.children.iter() }</span> }
}

#[derive(Properties, PartialEq)]
pub struct UnreadProps {
    pub count: u32,
}

/// The unread count chip. Announces as "12 unread messages", never a bare "12".
#[function_component(Unread)]
pub fn unread(p: &UnreadProps) -> Html {
    if p.count == 0 {
        return html! {};
    }
    html! {
        <span class="fn-unread" aria-label={crate::format::unread_label(p.count)}>
            { crate::format::unread_badge(p.count) }
        </span>
    }
}

#[derive(Properties, PartialEq)]
pub struct SpinnerProps {
    #[prop_or_default]
    pub large: bool,
    /// Use on an orange fill, where the default border colour disappears.
    #[prop_or_default]
    pub on_primary: bool,
    #[prop_or_default]
    pub label: Option<String>,
}

#[function_component(Spinner)]
pub fn spinner(p: &SpinnerProps) -> Html {
    let mut class = classes!("fn-spinner");
    if p.large {
        class.push("fn-spinner--lg");
    }
    if p.on_primary {
        class.push("fn-spinner--on-primary");
    }
    html! {
        <span {class} role="status" aria-label={p.label.clone().unwrap_or_else(|| "Loading".into())}></span>
    }
}

/// Skeleton rows. Shown only where the shape is known; the caller is
/// responsible for not rendering these before 400 ms (DESIGN.md §15).
#[derive(Properties, PartialEq)]
pub struct SkeletonProps {
    pub rows: usize,
}

#[function_component(Skeleton)]
pub fn skeleton(p: &SkeletonProps) -> Html {
    html! {
        <div aria-hidden="true">
            { for (0..p.rows).map(|i| html! { <div key={i} class="fn-skel" /> }) }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct EmptyProps {
    /// The emoji shown on the plate when there is no illustration.
    pub art: String,
    pub title: String,
    #[prop_or_default]
    pub description: Option<String>,
    /// Red wash, for a failure rather than an absence.
    #[prop_or_default]
    pub is_error: bool,
    /// An `fn-art--*` modifier (see app.css §11). When present the emoji tile
    /// becomes an illustration plate and the glyph stops being drawn; the art
    /// itself swaps light/dark from CSS.
    #[prop_or_default]
    pub art_class: Classes,
    #[prop_or_default]
    pub children: Children,
}

/// An empty or error state.
///
/// Every empty state names the next action, and every error states what failed
/// and what to do. Neither apologises, and neither says "something went wrong"
/// (DESIGN.md §15).
#[function_component(Empty)]
pub fn empty(p: &EmptyProps) -> Html {
    let class = if p.is_error {
        "fn-empty fn-empty--error"
    } else {
        "fn-empty"
    };
    let art_class = if p.art_class.is_empty() {
        classes!("fn-empty__art")
    } else {
        classes!("fn-empty__art", "fn-art", p.art_class.clone())
    };
    html! {
        <div {class} role={if p.is_error { "alert" } else { "note" }}>
            <div class={art_class} aria-hidden="true">{ &p.art }</div>
            <h2 class="fn-empty__title">{ &p.title }</h2>
            if let Some(d) = &p.description {
                <p class="fn-empty__desc">{ d }</p>
            }
            { for p.children.iter() }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct BannerProps {
    /// `warn`, `danger`, `info`, or `offline`.
    pub variant: &'static str,
    pub children: Children,
    /// Optional right-aligned action buttons ("Rotate now", "Try again").
    #[prop_or_default]
    pub actions: Option<Html>,
}

/// The strip under a screen header that explains why something is blocked.
#[function_component(Banner)]
pub fn banner(p: &BannerProps) -> Html {
    // `role="status"` rather than `alert` for warnings: these persist, and an
    // assertive live region that never goes away is a screen-reader trap.
    let role = if p.variant == "danger" {
        "alert"
    } else {
        "status"
    };
    html! {
        <div class={format!("fn-banner fn-banner--{}", p.variant)} {role}>
            <span>{ for p.children.iter() }</span>
            if let Some(a) = &p.actions {
                <span class="fn-banner__actions">{ a.clone() }</span>
            }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct ConnPillProps {
    pub status: ConnStatus,
    pub onclick: Callback<MouseEvent>,
}

/// The connection pill. It is a *control*, not just an indicator: clicking
/// toggles live ↔ polling, and clicking a failure state retries.
#[function_component(ConnPill)]
pub fn conn_pill(p: &ConnPillProps) -> Html {
    let lang = crate::state::use_store().language;
    html! {
        <button
            type="button"
            class={format!("fn-conn {}", p.status.pill_class())}
            aria-label={p.status.aria_label(lang)}
            title={p.status.aria_label(lang)}
            onclick={p.onclick.clone()}
        >
            { p.status.label(lang) }
        </button>
    }
}

#[derive(Properties, PartialEq)]
pub struct IconButtonProps {
    pub label: String,
    pub icon: Html,
    pub onclick: Callback<MouseEvent>,
    #[prop_or_default]
    pub disabled: bool,
    #[prop_or(true)]
    pub quiet: bool,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or_default]
    pub busy: bool,
}

/// An icon-only button.
///
/// `label` becomes both `title` and `aria-label`, and must name the **object**,
/// not the icon: "Remove sourCherry88 from this room", never "Minus".
#[function_component(IconButton)]
pub fn icon_button(p: &IconButtonProps) -> Html {
    let base = if p.quiet {
        "topcoat-icon-button--quiet"
    } else {
        "topcoat-icon-button"
    };
    html! {
        <button
            type="button"
            class={classes!(base, p.class.clone())}
            aria-label={p.label.clone()}
            title={p.label.clone()}
            disabled={p.disabled}
            aria-busy={p.busy.then_some("true")}
            onclick={p.onclick.clone()}
        >
            if p.busy {
                <Spinner />
            } else {
                { p.icon.clone() }
            }
        </button>
    }
}

/// Wrap bytes in an object URL, tagged with `mime`.
///
/// Shared by the Files drawer and the in-message attachment embed. Both need it
/// because an attachment download is authenticated: there is no URL a browser
/// can be pointed at directly, so the bytes are fetched and handed to the page
/// as a blob.
///
/// **Every caller owns a revoke.** An object URL pins its bytes for the life of
/// the document, so one that is created and forgotten is a memory leak the size
/// of the file.
#[cfg(target_arch = "wasm32")]
pub fn object_url(bytes: &[u8], mime: &str) -> Option<String> {
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn object_url(_bytes: &[u8], _mime: &str) -> Option<String> {
    None
}

/// Save `url` under `filename` by clicking a synthetic anchor.
///
/// The same trick the wallet backup uses, for the same reason: there is no other
/// way to name a download from script.
#[cfg(target_arch = "wasm32")]
pub fn save_as(url: &str, filename: &str) {
    use wasm_bindgen::JsCast;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Ok(el) = doc.create_element("a") else {
        return;
    };
    let Ok(anchor) = el.dyn_into::<web_sys::HtmlAnchorElement>() else {
        return;
    };
    anchor.set_href(url);
    anchor.set_download(filename);
    anchor.click();
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save_as(_url: &str, _filename: &str) {}

/// A copy-to-clipboard helper.
///
/// Falls back silently when the Clipboard API is unavailable (insecure origin,
/// or an older engine) — the caller shows a toast either way, and a failed copy
/// is not worth an error dialog.
#[cfg(target_arch = "wasm32")]
pub fn copy_to_clipboard(text: &str) -> bool {
    let Some(win) = web_sys::window() else {
        return false;
    };

    // The async Clipboard API exists **only in a secure context**. This app is
    // explicitly meant to be opened over a plain-http LAN address — that is the
    // URL the server prints for other people to use — and there
    // `navigator.clipboard` is `undefined`. Reaching through it threw
    // `TypeError: Cannot read properties of undefined (reading 'writeText')`,
    // which aborted the click handler before it could record that the phrase
    // had been backed up, leaving "Sign in" disabled forever. Creating an
    // account over the network was impossible.
    //
    // So: probe for the API rather than assuming it, and fall back to the
    // legacy selection-based copy, which has no secure-context requirement.
    let has_async_api = js_sys::Reflect::get(&win.navigator(), &JsValue::from_str("clipboard"))
        .map(|v| !v.is_undefined() && !v.is_null())
        .unwrap_or(false);

    if has_async_api {
        let _ = win.navigator().clipboard().write_text(text);
        return true;
    }

    legacy_copy(&win, text)
}

/// `document.execCommand("copy")` over a temporary, off-screen textarea.
///
/// Deprecated, and the only thing that works on an insecure origin. It must be
/// called synchronously from a user gesture, which every caller here is.
#[cfg(target_arch = "wasm32")]
fn legacy_copy(win: &web_sys::Window, text: &str) -> bool {
    use wasm_bindgen::JsCast;

    let Some(doc) = win.document() else {
        return false;
    };
    let Some(body) = doc.body() else {
        return false;
    };
    let Ok(el) = doc.create_element("textarea") else {
        return false;
    };
    let Ok(area) = el.dyn_into::<web_sys::HtmlTextAreaElement>() else {
        return false;
    };

    area.set_value(text);
    // Off-screen rather than `display:none`: a hidden element cannot be
    // selected, and an unselected one cannot be copied.
    let _ = area.set_attribute("style", "position:fixed;top:-1000px;opacity:0");
    let _ = area.set_attribute("readonly", "true");

    if body.append_child(&area).is_err() {
        return false;
    }
    area.select();
    let copied = doc
        .dyn_into::<web_sys::HtmlDocument>()
        .ok()
        .and_then(|d| d.exec_command("copy").ok())
        .unwrap_or(false);
    let _ = body.remove_child(&area);
    copied
}

#[cfg(not(target_arch = "wasm32"))]
pub fn copy_to_clipboard(_text: &str) -> bool {
    false
}

/// The "Skip to messages" link — the first focusable element on the page,
/// visible only when focused (DESIGN.md §17).
#[function_component(SkipLink)]
pub fn skip_link() -> Html {
    let lang = crate::state::use_store().language;
    html! {
        <a class="fn-sr-only" href="#fn-main">
            { crate::i18n::t(lang, crate::i18n::Key::skip_to_messages) }
        </a>
    }
}

/// A `topcoat-button` that shows a spinner while busy **without losing its
/// label** — swapping the label for "Loading…" makes the user lose track of
/// what they pressed (DESIGN.md §5).
#[derive(Properties, PartialEq)]
pub struct BusyButtonProps {
    pub label: String,
    pub onclick: Callback<MouseEvent>,
    #[prop_or_default]
    pub busy: bool,
    #[prop_or_default]
    pub disabled: bool,
    #[prop_or("topcoat-button--cta")]
    pub class: &'static str,
    #[prop_or_default]
    pub button_type: Option<&'static str>,
}

#[function_component(BusyButton)]
pub fn busy_button(p: &BusyButtonProps) -> Html {
    let on_primary = p.class.contains("cta") || p.class.contains("danger");
    html! {
        <button
            type={p.button_type.unwrap_or("button")}
            class={p.class}
            disabled={p.disabled || p.busy}
            aria-busy={p.busy.then_some("true")}
            onclick={p.onclick.clone()}
        >
            { &p.label }
            if p.busy {
                { " " }
                <Spinner on_primary={on_primary} />
            }
        </button>
    }
}

/// The offline banner, used verbatim on every screen so the copy cannot drift.
#[function_component(OfflineBanner)]
pub fn offline_banner() -> Html {
    let lang = crate::state::use_store().language;
    html! {
        <Banner variant="offline">
            { crate::i18n::t(lang, crate::i18n::Key::offline_banner) }
        </Banner>
    }
}

/// A back button for the mobile single-column layout.
#[derive(Properties, PartialEq)]
pub struct BackProps {
    pub onclick: Callback<MouseEvent>,
    #[prop_or("Back to rooms".to_string())]
    pub label: String,
}

#[function_component(Back)]
pub fn back(p: &BackProps) -> Html {
    html! {
        <button
            type="button"
            class="fn-back topcoat-icon-button--quiet"
            aria-label={p.label.clone()}
            title={p.label.clone()}
            onclick={p.onclick.clone()}
        >
            { icons::back(20) }
        </button>
    }
}

/// A popover that animates out before it disappears.
///
/// CSS cannot animate an element that is already gone, so an exit needs
/// something to keep the node mounted for the length of the animation. The
/// modal does this itself; menus and pickers are toggled by a plain `bool` in
/// whichever component owns them, so this wrapper carries the same behaviour
/// without each of them growing a copy of it.
///
/// The parent keeps its `bool` and keeps setting it exactly as before —
/// including to `false` — and renders this **unconditionally**. When `open`
/// goes false the wrapper holds the node for [`EXIT_MS`], stamped
/// `data-closing` for app.css to animate, and only then renders nothing.
#[derive(Properties, PartialEq)]
pub struct PopoverProps {
    pub open: bool,
    pub children: Children,
    #[prop_or_default]
    pub class: Classes,
    /// `menu` for the `⋮` lists, `dialog` for the emoticon grid — `.fn-picker`
    /// styles the two differently off this attribute.
    #[prop_or_default]
    pub role: Option<AttrValue>,
    #[prop_or_default]
    pub label: Option<AttrValue>,
    #[prop_or_default]
    pub onkeydown: Option<Callback<KeyboardEvent>>,
    /// Emitted when a click lands anywhere outside the open popover. The
    /// parent owns `open`, so the parent is who closes it — this is the
    /// light-dismiss every menu is expected to have.
    #[prop_or_default]
    pub on_dismiss: Option<Callback<()>>,
}

/// Matches `.fn-picker[data-closing]` in app.css §8. Shorter than the modal's:
/// a menu is a smaller object travelling a shorter distance, and the same
/// duration on it reads as sluggish rather than considered.
const POPOVER_EXIT_MS: u32 = 110;

#[function_component(Popover)]
pub fn popover(p: &PopoverProps) -> Html {
    // `mounted` exists only to hold the node through the EXIT animation.
    // Opening renders off `p.open` directly, in the same render pass as the
    // click — waiting for an effect to flip a state first costs an entire
    // effect-plus-render round trip, which is the difference between a menu
    // that answers the tap and one that follows it.
    let mounted = use_state(|| p.open);
    let closing = use_state(|| false);
    let node = use_node_ref();

    // Light dismiss: while open, a document-level click that is not inside
    // the popover emits `on_dismiss`.
    //
    // The listener starts DISARMED and arms only after a zero-delay timeout.
    // This is load-bearing: microtasks run between event listeners during
    // bubbling, so Yew renders (and this effect attaches the listener) while
    // the opening click is still travelling to the document — armed
    // immediately, the click that opened the menu would be the click that
    // closes it, and the menu would never appear at all. A macrotask runs
    // strictly after the in-flight event finishes, so arming there means
    // only *subsequent* clicks can dismiss.
    {
        let node = node.clone();
        let on_dismiss = p.on_dismiss.clone();
        use_effect_with(p.open, move |open| {
            let listener = (*open && on_dismiss.is_some())
                .then(|| {
                    let document = web_sys::window()?.document()?;
                    let armed = std::rc::Rc::new(std::cell::Cell::new(false));
                    {
                        let armed = armed.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            gloo_timers::future::TimeoutFuture::new(0).await;
                            armed.set(true);
                        });
                    }
                    Some(gloo_events::EventListener::new(
                        &document,
                        "click",
                        move |e: &web_sys::Event| {
                            use wasm_bindgen::JsCast;
                            if !armed.get() {
                                return;
                            }
                            let inside = e
                                .target()
                                .and_then(|t| t.dyn_into::<web_sys::Node>().ok())
                                .zip(node.cast::<web_sys::Node>())
                                .is_some_and(|(target, root)| root.contains(Some(&target)));
                            if !inside {
                                if let Some(cb) = &on_dismiss {
                                    cb.emit(());
                                }
                            }
                        },
                    ))
                })
                .flatten();
            move || drop(listener)
        });
    }

    {
        let mounted = mounted.clone();
        let closing = closing.clone();
        use_effect_with(p.open, move |open| {
            if *open {
                closing.set(false);
                mounted.set(true);
            } else if *mounted {
                closing.set(true);
                let mounted = mounted.clone();
                let closing = closing.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    exit_sleep(POPOVER_EXIT_MS).await;
                    mounted.set(false);
                    closing.set(false);
                });
            }
            || ()
        });
    }

    // Visible while open (immediately) or while playing the exit (`mounted`
    // lingers). This also defuses the stale-timer race: a leftover exit
    // timer flipping `mounted` cannot unmount a popover that is open again.
    if !p.open && !*mounted {
        return Html::default();
    }
    html! {
        <div
            ref={node}
            class={p.class.clone()}
            role={p.role.clone()}
            aria-label={p.label.clone()}
            // Guarded by `!p.open`: reopened mid-exit, the node must not
            // wear the exit animation for the frame before the effect
            // clears `closing`.
            data-closing={(*closing && !p.open).then_some("true")}
            onkeydown={p.onkeydown.clone().unwrap_or_default()}
        >
            { for p.children.iter() }
        </div>
    }
}

#[cfg(target_arch = "wasm32")]
async fn exit_sleep(ms: u32) {
    // Reduced motion: §17 flattens the animation, so waiting for it would be
    // waiting for nothing. Same reasoning as `modal.rs::exit_delay_ms`.
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
