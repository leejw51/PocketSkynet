//! Shared primitives: the identity tile, addresses, badges, spinners, empty and
//! error states, banners, skeletons and the connection pill.
//!
//! Every one of these maps 1:1 onto a class in `app.css` §4 and §11. Nothing
//! here invents styling; the components exist so the class strings, the ARIA
//! attributes and the accessible names are written once instead of at each of
//! the two hundred call sites that would otherwise get one of them wrong.

use pocketskynet_core::{PresenceStatus, WalletAddress};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsValue;
use yew::prelude::*;

use crate::i18n::{t, Key, Lang};
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

/// Whether this keystroke belongs to an IME still assembling a character.
///
/// Every Enter-submits handler over a text field must check this first. With a
/// Korean (or any CJK) IME, the Enter that *commits* the syllable being
/// composed arrives as a keydown too — acting on it sends the message while
/// the IME then deposits the half-built syllable into the freshly cleared
/// field, so the next Enter posts it again. `keyCode` 229 is the same signal
/// from browsers that predate `isComposing`.
pub fn ime_composing(e: &KeyboardEvent) -> bool {
    e.is_composing() || e.key_code() == 229
}

/// How long after a `compositionend` an Enter is still the IME's.
///
/// The two events are dispatched back to back in the same task, so the real
/// gap is under a millisecond; 50 ms is slack, and still far below the ~100 ms
/// a person needs to decide to press Enter a second time.
const COMPOSITION_TAIL_MS: f64 = 50.0;

/// Composition state for a field where Enter submits.
///
/// [`ime_composing`] alone is not enough, because the two engines order the
/// events differently. Chrome and Firefox fire `keydown` *before*
/// `compositionend` and flag it `isComposing`, so the flag catches it. WebKit
/// fires `compositionend` *first* — by the time `keydown` arrives the
/// composition is already over and nothing on the event distinguishes it from
/// a deliberate press. The timestamp is what covers that second ordering.
#[derive(Clone, PartialEq)]
pub struct ImeGuard {
    ended_at: std::rc::Rc<std::cell::RefCell<f64>>,
}

/// Per-field composition tracking, watching the field behind `field`.
///
/// The listener is attached by hand because Yew 0.21 has no `oncompositionend`
/// attribute — composition events are simply absent from its event table.
#[hook]
pub fn use_ime_guard(field: NodeRef) -> ImeGuard {
    let ended_at = use_mut_ref(|| 0f64);
    {
        let ended_at = ended_at.clone();
        use_effect_with(field, move |field| {
            let listener = field.get().map(|target| {
                gloo_events::EventListener::new(&target, "compositionend", move |_| {
                    *ended_at.borrow_mut() = js_sys::Date::now();
                })
            });
            move || drop(listener)
        });
    }
    ImeGuard { ended_at }
}

impl ImeGuard {
    /// Whether this keystroke belongs to the IME rather than to the app.
    pub fn blocks(&self, e: &KeyboardEvent) -> bool {
        ime_composing(e) || js_sys::Date::now() - *self.ended_at.borrow() < COMPOSITION_TAIL_MS
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
    /// A quieter second line: a role or a description. **Not** an address.
    pub subtitle: Option<String>,
    /// The wallet behind this tile, when there is one. Rooms pass `None`.
    ///
    /// The address itself rather than a formatted string, because the stage
    /// shows it two ways — abbreviated, then in full behind a reveal — and
    /// copies a third. Handing it a `String` would mean deciding here which
    /// of the three the caller meant, which is how the truncated form ended
    /// up un-checksummed in the first place.
    pub address: Option<WalletAddress>,
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
    /// The presence dot: filled for online, a ring for away, nothing at all for
    /// offline (`store.presence_of`).
    ///
    /// Defaults to `Offline`, which draws nothing — so a caller that has no
    /// presence to hand shows none, rather than the unconditional green dot the
    /// React client paints on every avatar whether or not anybody is there.
    #[prop_or(PresenceStatus::Offline)]
    pub presence: PresenceStatus,
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
    // Read from the store rather than taken as a prop, for the same reason
    // `Addr` below does: the skin is document-wide, so threading it through
    // fourteen call sites would be fourteen chances to forget one and render
    // the other skin's face in a corner nobody looks at.
    let skin = crate::state::use_store().skin;
    let monogram = identity::monogram_for(&p.seed);
    let hue = identity::hue_for(&p.seed);
    // A chosen avatar wins over the hash-derived face; the coloured tile and
    // monogram stay underneath either way, as the loading/failure fallback.
    let src = p
        .image
        .as_deref()
        .and_then(|i| identity::avatar_src(skin, i))
        .unwrap_or_else(|| crate::asset::img(skin, identity::art_for(&p.seed)));
    let mut class = classes!("fn-ident", "fn-ident--art", p.class.clone());
    class.push(p.size.class().trim());
    if p.is_self {
        class.push("fn-ident--self");
    }
    match p.presence {
        PresenceStatus::Online => class.push("fn-ident--online"),
        PresenceStatus::Away => class.push("fn-ident--away"),
        // Nothing. See `IdentProps::presence`.
        PresenceStatus::Offline => {}
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
                skin,
                &seed,
                image.as_deref(),
                zoom.title.clone(),
                zoom.subtitle.clone(),
                zoom.address.clone(),
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
    /// Show the full EIP-55 checksum rather than `0x9f2a…7C41`.
    ///
    /// Mutually exclusive with [`Self::revealable`], which is the same idea
    /// with a switch on it; setting both is treated as `revealable`, since a
    /// reveal control that starts revealed has nothing to reveal.
    #[prop_or_default]
    pub full: bool,
    /// Start truncated, with an eye button that shows the whole thing.
    ///
    /// Every place an address is printed at full length is a place where
    /// forty-two mono characters — the longest string in the product — sit
    /// next to something they are not more important than: a username, a
    /// recovery-phrase warning, a sentence someone typed. The short form is
    /// what an address is *recognised* by; the full form is what it is
    /// *verified* by, and verification is a thing you deliberately go and do.
    ///
    /// So the default became "recognisable", with the full string one tap
    /// away. The clipboard is unaffected either way — copying always yields
    /// the complete checksum, so the common case never needs the reveal at
    /// all.
    #[prop_or_default]
    pub revealable: bool,
    /// Tapping the address copies the **full checksum**, truncated or not.
    ///
    /// On by default. An address on screen is nearly always something someone
    /// is about to paste somewhere else, and asking them to select forty-two
    /// mono characters by hand — on a phone — is the kind of small friction
    /// that makes a product feel unfinished. Turn it off only where the
    /// address sits *inside* another control: a button within a button is not
    /// markup a browser has an answer for.
    #[prop_or(true)]
    pub copy: bool,
}

/// How long the copied tick stays lit. Long enough to be read after the
/// pointer has moved on, short enough that the next copy is a fresh event.
const COPIED_FLASH_MS: u32 = 1200;

/// A wallet address in mono — and, by default, the control that copies it.
///
/// The `aria-label` always carries the **full checksum** even when the visible
/// text is truncated, so a screen-reader user can read out the whole address
/// (DESIGN.md §17). EIP-55 casing is display-only; comparisons elsewhere use
/// the lowercase form the newtype guarantees.
#[function_component(Addr)]
pub fn addr(p: &AddrProps) -> Html {
    let store = crate::state::use_store();
    let lang = store.language;
    // Unconditional, and before the read-only early return: hooks are ordered
    // by call, not by branch.
    let copied = use_state(|| false);
    let revealed = use_state(|| false);

    let checksum = p.address.to_checksummed();
    // `revealable` wins over `full`: see the prop docs — a reveal control that
    // starts revealed is a control with nothing to do.
    let showing_full = if p.revealable { *revealed } else { p.full };
    let text = if showing_full {
        checksum.clone()
    } else {
        // Sliced out of the checksum, never out of the stored lowercase. The
        // two forms are shown side by side often enough — here and in the
        // spotlight — that disagreeing casing reads as one of them being the
        // wrong address.
        p.address.abbreviated_checksummed()
    };
    let mut class = classes!("fn-addr");
    if showing_full {
        class.push("fn-addr--full");
    }

    // The eye, when this instance offers one. Built before the `!p.copy`
    // return so a read-only address can still be revealed — the two are
    // independent: one is about the clipboard, the other about the screen.
    let eye = p.revealable.then(|| {
        let onclick = {
            let revealed = revealed.clone();
            Callback::from(move |e: MouseEvent| {
                // The address usually sits inside something with its own tap —
                // a members row, a room card. Neither that nor the copy gesture
                // beside it should fire because someone asked to see the rest.
                e.stop_propagation();
                revealed.set(!*revealed);
            })
        };
        let key = if *revealed {
            Key::hide_full_address
        } else {
            Key::view_full_address
        };
        html! {
            <button
                type="button"
                class="topcoat-icon-button--quiet fn-addr__reveal"
                aria-pressed={revealed.to_string()}
                title={t(lang, key)}
                aria-label={t(lang, key)}
                {onclick}
            >{ if *revealed { icons::eye_off(14) } else { icons::eye(14) } }</button>
        }
    });

    // An inline-flex wrapper rather than a block: this whole assembly can land
    // mid-sentence inside a message bubble, and it has to flow with the words
    // either side of it exactly as the bare span did.
    let wrap = |inner: Html| match &eye {
        None => inner,
        Some(eye) => html! {
            <span class="fn-addr-group">{ inner }{ eye.clone() }</span>
        },
    };

    if !p.copy {
        let aria = t(lang, Key::wallet_address_aria).replace("{address}", &checksum);
        return wrap(html! { <span {class} aria-label={aria}>{ text }</span> });
    }

    class.push("fn-addr--copy");
    let label = t(lang, Key::copy_wallet_address).replace("{address}", &checksum);

    let fire = {
        let store = store.clone();
        let checksum = checksum.clone();
        let copied = copied.clone();
        std::rc::Rc::new(move || {
            copy_with_toast(&store, &checksum, t(lang, Key::address_copied));
            copied.set(true);
            let copied = copied.clone();
            wasm_bindgen_futures::spawn_local(async move {
                flash_sleep(COPIED_FLASH_MS).await;
                copied.set(false);
            });
        })
    };
    let onclick = {
        let fire = fire.clone();
        Callback::from(move |e: MouseEvent| {
            // Rows carry their own tap action — a members card copies too, a
            // room row opens the room. The address is the more specific
            // answer, so it takes the click rather than sharing it.
            e.stop_propagation();
            fire();
        })
    };
    let onkeydown = Callback::from(move |e: KeyboardEvent| {
        if e.key() == "Enter" || e.key() == " " {
            e.prevent_default();
            e.stop_propagation();
            fire();
        }
    });

    wrap(html! {
        // A `<span role="button">` rather than a `<button>`, and the reason is
        // typographic: an address inside a message bubble is part of a
        // sentence. A real button is an atomic inline-block — forty-two mono
        // characters of it take a line of their own and push the rest of the
        // sentence onto the next one. A span flows, breaks and re-joins like
        // the text it is sitting in, and the role, the tabindex and the
        // Enter/Space handler give back everything the element gave up.
        //
        // The eye beside it *is* a real `<button>`, and can be: it is one
        // glyph wide, so it has no line of its own to take.
        <span
            {class}
            role="button"
            tabindex="0"
            data-copied={copied.then_some("true")}
            title={label.clone()}
            aria-label={label}
            {onclick}
            {onkeydown}
        >{ text }</span>
    })
}

#[cfg(target_arch = "wasm32")]
async fn flash_sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

#[cfg(not(target_arch = "wasm32"))]
async fn flash_sleep(_ms: u32) {}

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

/// The word for a presence status, in the reader's language.
pub fn presence_word(lang: Lang, status: PresenceStatus) -> &'static str {
    match status {
        PresenceStatus::Online => t(lang, Key::presence_online),
        PresenceStatus::Away => t(lang, Key::presence_away),
        PresenceStatus::Offline => t(lang, Key::presence_offline),
    }
}

#[derive(Properties, PartialEq)]
pub struct PresenceLabelProps {
    pub status: PresenceStatus,
    /// Draw it only for a screen reader. The dot on the avatar is the sighted
    /// reader's version, and in a dense list — a row of rooms, a message
    /// gutter — a second textual copy of it is clutter. In the members list,
    /// where the row is already three lines, the word is shown.
    #[prop_or_default]
    pub quiet: bool,
}

/// The word beside the dot.
///
/// Offline renders nothing in either mode: it is the state most people are in
/// most of the time, and labelling every absent colleague turns a roster into a
/// wall of "Offline". Its absence is legible on its own — no dot, no word.
#[function_component(PresenceLabel)]
pub fn presence_label(p: &PresenceLabelProps) -> Html {
    let lang = crate::state::use_store().language;
    if p.status == PresenceStatus::Offline {
        return html! {};
    }
    let word = presence_word(lang, p.status);
    if p.quiet {
        return html! { <span class="fn-sr-only">{ word }</span> };
    }
    let class = match p.status {
        PresenceStatus::Online => "fn-presence fn-presence--online",
        _ => "fn-presence",
    };
    html! { <span {class}>{ word }</span> }
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
pub struct MentionBadgeProps {
    pub count: u32,
}

/// The "@" chip that says some of the unread is addressed to you.
///
/// Deliberately separate from [`Unread`] rather than folded into it. They
/// answer different questions — "is there anything new here" and "is any of it
/// mine" — and it is the second one people triage by, so it has to survive
/// being next to a count of forty. Rendering it as a different shape, not a
/// differently-coloured number, is what makes that work at a glance.
#[function_component(MentionBadge)]
pub fn mention_badge(p: &MentionBadgeProps) -> Html {
    if p.count == 0 {
        return html! {};
    }
    let label = if p.count == 1 {
        "1 message mentions you".to_owned()
    } else {
        format!("{} messages mention you", p.count)
    };
    html! {
        <span class="fn-mention-badge" aria-label={label.clone()} title={label}>
            { "@" }
            if p.count > 1 {
                <span class="fn-mention-badge__count">
                    { crate::format::unread_badge(p.count) }
                </span>
            }
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
/// Async because the honest answer is: `writeText` returns a promise, and
/// whether the write was permitted is not known until it settles. This used to
/// fire that promise, discard it and return `true` — so a browser that denied
/// clipboard access (no permission, an unfocused document, Safari's stricter
/// gesture rules) still reported a successful copy. On the login screen that
/// was the difference between "your recovery phrase is on the clipboard" and
/// an account nobody can ever get back into.
#[cfg(target_arch = "wasm32")]
pub async fn copy_to_clipboard(text: &str) -> bool {
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

    if !has_async_api {
        // Nothing is awaited on this path, so the legacy copy still runs inside
        // the click's own turn — which it requires.
        return legacy_copy(&win, text);
    }

    let promise = win.navigator().clipboard().write_text(text);
    if wasm_bindgen_futures::JsFuture::from(promise).await.is_ok() {
        return true;
    }

    // Denied. The legacy path wants a live user gesture and we have just
    // awaited past it, so this is best effort — but it either copies or
    // reports that it did not, which is the whole point.
    legacy_copy(&win, text)
}

/// Copy `text`, then hand the outcome — the real one — to `done`.
///
/// The shape every caller wants: they are inside a click handler, not an async
/// context, and they need to know whether it worked.
#[cfg(target_arch = "wasm32")]
pub fn copy_then(text: &str, done: impl FnOnce(bool) + 'static) {
    let text = text.to_owned();
    wasm_bindgen_futures::spawn_local(async move { done(copy_to_clipboard(&text).await) });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn copy_then(_text: &str, done: impl FnOnce(bool) + 'static) {
    done(false);
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
pub async fn copy_to_clipboard(_text: &str) -> bool {
    false
}

/// Copy `text`, then say so.
///
/// The one place the confirmation and the "clipboard blocked" failure are
/// worded. Copying is the most repeated micro-interaction in this client —
/// addresses, hashes, phrases — and a copy that reports success in three
/// different sentences reads as three different features.
pub fn copy_with_toast(store: &crate::state::Store, text: &str, title: impl Into<String>) {
    if text.is_empty() {
        return;
    }
    let store = store.clone();
    let title = title.into();
    copy_then(text, move |ok| {
        if ok {
            super::toast::success(&store, title);
        } else {
            let lang = store.language;
            super::toast::error(
                &store,
                t(lang, Key::couldnt_copy),
                Some(t(lang, Key::clipboard_blocked).into()),
            );
        }
    });
}

/// True when a click landed on — or inside — something that answers clicks by
/// itself: a button, a link, a field, or anything wearing `role="button"`.
///
/// Rows that carry a tap action of their own need this. A members card copies
/// the address, but the block and remove buttons on its trailing edge are not
/// "the card", and a copy that also fired on *Block* would be two answers to
/// one tap.
#[cfg(target_arch = "wasm32")]
pub fn hit_control(e: &MouseEvent) -> bool {
    use wasm_bindgen::JsCast;
    e.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| {
            el.closest("button, a, input, textarea, select, [role='button']")
                .ok()
                .flatten()
        })
        .is_some()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn hit_control(_e: &MouseEvent) -> bool {
    false
}

/// The translated name of a skin.
///
/// Lives here rather than on [`Skin`] itself so `session.rs` — which is the
/// persistence layer — stays free of i18n, and here rather than inline at the
/// two pickers so adding a third skin is one arm in one file instead of a
/// compile error in one picker and a silent omission in the other.
pub fn skin_label(lang: crate::i18n::Lang, skin: crate::session::Skin) -> &'static str {
    use crate::i18n::Key;
    crate::i18n::t(
        lang,
        match skin {
            crate::session::Skin::Skynet => Key::skin_skynet,
            crate::session::Skin::Cute => Key::skin_cute,
            crate::session::Skin::Human => Key::skin_human,
        },
    )
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

/// Hold a leaving node for the length of its exit animation.
///
/// Shared with the lightbox, which has the same problem the popover does: Yew
/// unmounts between two frames, so anything that animates *out* has to be kept
/// mounted by Rust for exactly as long as CSS says the exit runs.
#[cfg(target_arch = "wasm32")]
pub(super) async fn exit_sleep(ms: u32) {
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
pub(super) async fn exit_sleep(_ms: u32) {}
