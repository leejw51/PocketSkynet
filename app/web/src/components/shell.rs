//! The app shell: testnet ribbon, top bar, two-pane layout, bottom nav
//! (DESIGN.md §4, §16).
//!
//! The list pane is **always mounted** at every authenticated route. On a
//! narrow viewport it is hidden with `display:none` (via
//! `.fn-panes[data-view]`) rather than unmounted, so scroll position, the
//! realtime subscription and the decrypted message caches survive navigation.
//! Unmounting it would make every "back" a full re-sync.

use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::route::Route;
use crate::session::{ShellLayout, Theme};
use crate::state::{use_store, Action, Confirm, ConfirmAction, Modal};

use super::common::{Addr, Ident, IdentSize, Unread};
use super::icons;

#[derive(Properties, PartialEq)]
pub struct ShellProps {
    pub route: Route,
    pub on_navigate: Callback<Route>,
    /// The persistent left rail.
    pub list: Html,
    /// Chat, members, invitations or settings.
    pub detail: Html,
}

#[function_component(Shell)]
pub fn shell(p: &ShellProps) -> Html {
    let store = use_store();
    let lang = store.language;

    // Which transport is actually carrying this session.
    //
    // Asked once per mount, and only ever *displayed* when the answer is
    // HTTP/3 — a browser upgrades itself silently once it has seen `Alt-Svc`,
    // so the interesting event is the upgrade, and a badge that also said
    // "HTTP/2" on every ordinary page would be noise nobody reads.
    let on_http3 = use_state(|| false);
    {
        let store = store.clone();
        let on_http3 = on_http3.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(info) = store.client.server_info().await {
                    on_http3.set(info.is_http3());
                    // The same call carries the address to hand to other
                    // people, which everything that offers a "copy link"
                    // reads out of the store (`AppState::shareable_url`).
                    store.dispatch(crate::state::Action::SetShareBase(info.share_base));
                }
            });
            || ()
        });
    }

    // Tapping your own portrait raises the spotlight — the zoomed portrait
    // with the light swarm. The copy gesture this button used to be lives on
    // inside the stage as a labelled button, which is strictly clearer than
    // a silent copy-on-click ever was.
    let identity_click = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            if let Some(a) = store.auth.address() {
                let seed = a.to_string();
                super::spotlight::show(super::spotlight::Spot {
                    // The chosen profile image, when there is one — the
                    // spotlight is the zoomed view of the tile beside it, and
                    // the two showing different faces reads as a bug.
                    image: store
                        .auth
                        .profile_image()
                        .and_then(crate::identity::avatar_src)
                        .unwrap_or_else(|| {
                            format!("/static/img/{}.png", crate::identity::art_for(&seed))
                        }),
                    title: store.auth.username().unwrap_or_default().to_owned(),
                    subtitle: Some(a.to_checksummed()),
                    copy: Some(a.to_checksummed()),
                    hue: crate::identity::hue_for(&seed),
                });
            }
        })
    };

    let cycle_theme = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            let next = match store.theme {
                Theme::System => Theme::Light,
                Theme::Light => Theme::Dark,
                Theme::Dark => Theme::System,
            };
            store.dispatch(Action::SetTheme(next));
        })
    };

    // The top-bar twin of the Settings → Layout row. Two states, so this is a
    // toggle rather than a pick list: an icon-only control in a bar of five
    // has no room to show two options, and flipping between them is the whole
    // interaction anyway.
    let toggle_layout = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            let next = match store.shell_layout {
                ShellLayout::Horizontal => ShellLayout::Vertical,
                ShellLayout::Vertical => ShellLayout::Horizontal,
            };
            store.dispatch(Action::SetShellLayout(next));
        })
    };

    // Font controls: one click steps to the next face / size, like the
    // appearance toggle beside them. The full pick lists live in Settings.
    let cycle_font_face = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            store.dispatch(Action::SetFontFace(store.font_face.next()));
        })
    };
    let cycle_font_scale = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            store.dispatch(Action::SetFontScale(store.font_scale.next()));
        })
    };

    let go = |route: Route, on_navigate: Callback<Route>| {
        Callback::from(move |_: MouseEvent| on_navigate.emit(route.clone()))
    };

    // Sign out is one click from every screen — it used to live only at the
    // bottom of Settings, which made it hard to find. Same confirmation
    // Settings uses, so leaving is never a single accidental click either.
    let sign_out_click = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            store.dispatch(Action::OpenModal(Modal::Confirm(Confirm {
                title: t(lang, Key::sign_out_title).into(),
                // Accurate about what sign-out removes: the remembered
                // credential (vault) and the cached rooms go; a wallet backup
                // file the user downloaded is not ours to touch.
                body: t(lang, Key::sign_out_body).into(),
                confirm_label: t(lang, Key::sign_out).into(),
                action: ConfirmAction::SignOut,
            })));
        })
    };

    let address = store.auth.address().cloned();
    let username = store.auth.username().unwrap_or("").to_owned();
    let invites = store.pending_invitations();
    let unread = store.total_unread();
    // Today's unfinished orders. Read straight from local storage rather than
    // held in the store: it changes only when an award lands, and the nav
    // re-renders on every one of those anyway.
    let orders_left = {
        let file = crate::progression::Progression::load_stored();
        (file.today().len() - file.completed_today()) as u32
    };
    let room = p.route.room_id().cloned();

    // A fact about the environment, not a notification, and never dismissible.
    // It used to appear only on a testnet, because a testnet was the default
    // and mainnet was the deliberate act. That is now the other way round, so
    // the strip has to state the riskier case too: on mainnet the wallet
    // spends real money, and "no ribbon at all" would be the same silence a
    // testnet used to earn. Falls back to `/blockchain/info` before the
    // registry has answered, so the strip is never blank or wrong.
    let ribbon = {
        let net = store.active_network();
        let mainnet = net.map(|n| !n.testnet).unwrap_or(!store.chain.is_testnet());
        let label = net
            .map(|n| n.name.to_uppercase())
            .unwrap_or_else(|| store.chain.chain_name.to_uppercase());
        html! {
            <div class="fn-ribbon" data-live={mainnet.then_some("true")} role="note">
                { if mainnet {
                    format!("LIVE NETWORK · {label} · REAL FUNDS")
                } else {
                    format!("TESTNET ENVIRONMENT · {label}")
                } }
            </div>
        }
    };

    html! {
        <div class="fn-app">
            <super::common::SkipLink />

            { ribbon }

            <header class="fn-topbar" role="banner">
                <button
                    type="button"
                    class="fn-topbar__identity"
                    onclick={identity_click}
                    aria-label={address.as_ref().map(|a| format!(
                        "Signed in as {username}, wallet address {}. View profile.",
                        a.to_checksummed()
                    ))}
                >
                    if let Some(a) = &address {
                        <Ident seed={a.to_string()} size={IdentSize::Sm} is_self=true image={store.auth.profile_image().map(str::to_owned)} />
                    }
                    <span>
                        <span class="fn-topbar__name">{ &username }</span>
                        <span class="fn-topbar__addr">
                            if let Some(a) = &address {
                                // `copy=false`: this one is inside the profile
                                // button. Nested buttons are invalid markup,
                                // and the tap already has an owner.
                                <Addr address={a.clone()} copy=false />
                            }
                        </span>
                    </span>
                </button>

                // The running build's version, straight from Cargo — the
                // workspace bump is the only place the number lives.
                <span class="fn-topbar__version">
                    { concat!("v", env!("CARGO_PKG_VERSION")) }
                </span>

                // Only when it is true. The badge is a fact about this
                // connection, not a feature advertisement — on HTTP/1.1 or
                // HTTP/2 there is nothing to say here, and the Server dialog
                // carries the full picture either way.
                if *on_http3 {
                    <button
                        type="button"
                        class="fn-topbar__h3"
                        title={"Connected over HTTP/3 (QUIC). Open server info."}
                        aria-label={"Connected over HTTP/3 over QUIC. Open server info."}
                        onclick={{
                            let store = store.clone();
                            Callback::from(move |_: MouseEvent| {
                                store.dispatch(Action::OpenModal(crate::state::Modal::ServerInfo));
                            })
                        }}
                    >
                        { "HTTP/3" }
                    </button>
                }

                <nav class="fn-topbar__actions" aria-label={t(lang, Key::nav_account)}>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wallet"
                        aria-label={store
                            .active_network()
                            .map(|n| t(lang, Key::wallet_nav_label).replace("{network}", &n.name))
                            .unwrap_or_else(|| "Wallet".to_owned())}
                        title={t(lang, Key::wallet)}
                        onclick={{
                            let store = store.clone();
                            Callback::from(move |_: MouseEvent| {
                                store.dispatch(Action::OpenModal(crate::state::Modal::Wallet));
                            })
                        }}
                    >
                        { icons::wallet(18) }
                    </button>
                    // Destinations first — Wallet, Knowledge, Bank, Publish,
                    // Operator — then the modals and the appearance toggles.
                    // A row that interleaves "go somewhere" with "change how
                    // this looks" makes people read all fourteen glyphs to
                    // find one; grouped, they only read the half they want.
                    //
                    // Everything from here down carries `fn-topbar__wide` and
                    // is hidden below 800px, where the bottom nav and the More
                    // sheet behind its fifth tab reach all of it. Fourteen
                    // 36px buttons need 504px; a phone in portrait has 390,
                    // and the overflow ran off the screen edge taking sign-out
                    // — the last button in the row — with it.
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::nav_knowledge)}
                        title={t(lang, Key::nav_knowledge)}
                        aria-current={(p.route.nav_key() == "knowledge").then_some("page")}
                        onclick={go(Route::Knowledge, p.on_navigate.clone())}
                    >
                        { icons::book(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::menu_bank)}
                        title={t(lang, Key::menu_bank)}
                        aria-current={(p.route.nav_key() == "bank").then_some("page")}
                        onclick={go(Route::Bank, p.on_navigate.clone())}
                    >
                        { icons::bank(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::nav_publish)}
                        title={t(lang, Key::nav_publish)}
                        aria-current={(p.route.nav_key() == "publish").then_some("page")}
                        onclick={go(Route::Publish, p.on_navigate.clone())}
                    >
                        { icons::globe(18) }
                    </button>
                    // The operator's file. Also in the bottom nav, which the
                    // two-pane tier hides — without this button the whole
                    // section is unreachable on anything wider than a phone.
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::nav_operator)}
                        title={t(lang, Key::nav_operator)}
                        aria-current={(p.route.nav_key() == "operator").then_some("page")}
                        onclick={go(Route::Operator, p.on_navigate.clone())}
                    >
                        { icons::crown(18) }
                        <Unread count={orders_left} />
                    </button>
                    // Where this server is, and which transport is carrying
                    // this session. The second half is the reason it exists:
                    // a browser upgrades itself to HTTP/3 silently, so the
                    // page cannot tell without asking the server.
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide fn-topbar__server"
                        aria-label={t(lang, Key::server_info)}
                        title={t(lang, Key::server_info)}
                        onclick={{
                            let store = store.clone();
                            Callback::from(move |_: MouseEvent| {
                                store.dispatch(Action::OpenModal(crate::state::Modal::ServerInfo));
                            })
                        }}
                    >
                        { icons::server(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide fn-topbar__shout"
                        aria-label={t(lang, Key::shout_title)}
                        title={t(lang, Key::shout_title)}
                        onclick={{
                            let store = store.clone();
                            Callback::from(move |_: MouseEvent| {
                                store.dispatch(Action::OpenModal(crate::state::Modal::Shout));
                            })
                        }}
                    >
                        { icons::megaphone(18) }
                    </button>
                    // Hidden below the two-pane breakpoint by its class: a
                    // phone has one column whichever way this points, and a
                    // control that visibly does nothing teaches people to stop
                    // believing the controls.
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__layout"
                        aria-label={match store.shell_layout {
                            ShellLayout::Horizontal => "Layout: side by side. Switch to stacked.",
                            ShellLayout::Vertical => "Layout: stacked. Switch to side by side.",
                        }}
                        title={t(lang, Key::layout)}
                        onclick={toggle_layout}
                    >
                        // The glyph shows the arrangement you are *in*, matching
                        // the appearance button beside it, whose label reads
                        // "Appearance: dark" rather than naming the other one.
                        { match store.shell_layout {
                            ShellLayout::Horizontal => icons::columns(18),
                            ShellLayout::Vertical => icons::rows(18),
                        } }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::appearance_change).replace("{theme}", store.theme.as_str())}
                        title={t(lang, Key::appearance)}
                        onclick={cycle_theme}
                    >
                        { icons::moon_sun(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::font_change).replace("{font}", store.font_face.as_str())}
                        title={t(lang, Key::font_face)}
                        onclick={cycle_font_face}
                    >
                        { icons::type_face(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::text_size_change).replace("{size}", store.font_scale.as_str())}
                        title={t(lang, Key::text_size)}
                        onclick={cycle_font_scale}
                    >
                        { icons::type_size(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={if invites == 1 {
                            "Invitations, 1 pending".to_owned()
                        } else {
                            t(lang, Key::invitations_pending).replace("{n}", &invites.to_string())
                        }}
                        title={t(lang, Key::invitations)}
                        onclick={go(Route::Invitations, p.on_navigate.clone())}
                    >
                        { icons::envelope(18) }
                        <Unread count={invites} />
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__wide"
                        aria-label={t(lang, Key::nav_settings)}
                        title={t(lang, Key::nav_settings)}
                        onclick={go(Route::Settings, p.on_navigate.clone())}
                    >
                        { icons::gear(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__signout"
                        aria-label={t(lang, Key::sign_out)}
                        title={t(lang, Key::sign_out)}
                        onclick={sign_out_click}
                    >
                        { icons::power(18) }
                    </button>
                </nav>
            </header>

            <div
                class="fn-panes"
                data-view={p.route.pane_view()}
                // Only consulted by the two-pane media tier (app.css §16);
                // a phone has one column no matter what this says.
                data-shell={store.shell_layout.as_str()}
            >
                <aside class="fn-pane fn-pane--list" aria-label={t(lang, Key::nav_rooms)}>
                    { p.list.clone() }
                </aside>
                <main class="fn-pane fn-pane--detail" id="fn-main">
                    // Keyed on the nav section, not on the route: moving
                    // between rooms keeps the same node (and so the same
                    // scroll position), while switching section remounts and
                    // replays the cross-fade-and-lift entrance (app.css §14).
                    <div class="fn-view" key={p.route.nav_key()}>
                        { p.detail.clone() }
                    </div>
                </main>
            </div>

            // Five slots, and only five. Eight tracks across 390pt gave each
            // label 48px, which is where "오퍼레이터" became "오퍼…" and the row
            // stopped being readable at a glance. The four constants stay —
            // rooms, this conversation, its members, the operator's file — and
            // the fifth is the door to everything else.
            <nav class="fn-bottomnav" aria-label={t(lang, Key::nav_sections)}>
                { nav_item(&p.route, "rooms", t(lang, Key::nav_rooms), icons::chat(20), Some(unread),
                           false, Route::Rooms, &p.on_navigate) }
                { nav_item(&p.route, "chat", t(lang, Key::nav_chat), icons::chat(20), None,
                           room.is_none(),
                           room.clone().map(Route::Room).unwrap_or(Route::Rooms), &p.on_navigate) }
                { nav_item(&p.route, "members", t(lang, Key::nav_members), icons::people(20), None,
                           room.is_none(),
                           room.clone().map(Route::Members).unwrap_or(Route::Rooms), &p.on_navigate) }
                { nav_item(&p.route, "operator", t(lang, Key::nav_operator), icons::crown(20), Some(orders_left),
                           false, Route::Operator, &p.on_navigate) }
                // Marked current whenever the open screen is one that lives
                // behind it, so the bar never claims you are nowhere. It
                // carries the invitations badge for the same reason: that
                // count is the one thing in there that asks for an answer,
                // and it would otherwise be invisible until you looked.
                <button
                    type="button"
                    class="fn-bottomnav__item"
                    aria-current={IN_MORE.contains(&p.route.nav_key()).then_some("page")}
                    aria-haspopup="dialog"
                    onclick={{
                        let store = store.clone();
                        Callback::from(move |_: MouseEvent| {
                            store.dispatch(Action::OpenModal(Modal::More));
                        })
                    }}
                >
                    { icons::ellipsis(20) }
                    <span>{ t(lang, Key::nav_more) }</span>
                    if invites > 0 {
                        <span class="fn-bottomnav__badge"><Unread count={invites} /></span>
                    }
                </button>
            </nav>
        </div>
    }
}

/// The sections the bottom nav reaches only through More. Kept beside the nav
/// rather than on `Route`, because it is a fact about this five-slot bar and
/// not about the routes themselves — the two-pane tier shows all of these as
/// top-bar buttons and has no More at all.
const IN_MORE: [&str; 5] = ["invites", "knowledge", "publish", "bank", "settings"];

/// One bottom-nav item.
///
/// Chat and Members are `disabled` with no room selected — a nav item that
/// navigates nowhere is worse than one that says it cannot.
#[allow(clippy::too_many_arguments)]
fn nav_item(
    route: &Route,
    key: &'static str,
    label: &'static str,
    icon: Html,
    badge: Option<u32>,
    disabled: bool,
    target: Route,
    on_navigate: &Callback<Route>,
) -> Html {
    let active = route.nav_key() == key;
    let onclick = {
        let on_navigate = on_navigate.clone();
        Callback::from(move |_: MouseEvent| on_navigate.emit(target.clone()))
    };
    html! {
        <button
            type="button"
            class="fn-bottomnav__item"
            aria-current={active.then_some("page")}
            disabled={disabled}
            {onclick}
        >
            { icon }
            <span>{ label }</span>
            if let Some(n) = badge.filter(|n| *n > 0) {
                <span class="fn-bottomnav__badge"><Unread count={n} /></span>
            }
        </button>
    }
}
