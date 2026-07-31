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
                                <Addr address={a.clone()} />
                            }
                        </span>
                    </span>
                </button>

                // The running build's version, straight from Cargo — the
                // workspace bump is the only place the number lives.
                <span class="fn-topbar__version">
                    { concat!("v", env!("CARGO_PKG_VERSION")) }
                </span>

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
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-topbar__shout"
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
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::menu_bank)}
                        title={t(lang, Key::menu_bank)}
                        aria-current={(p.route.nav_key() == "bank").then_some("page")}
                        onclick={go(Route::Bank, p.on_navigate.clone())}
                    >
                        { icons::bank(18) }
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
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::appearance_change).replace("{theme}", store.theme.as_str())}
                        title={t(lang, Key::appearance)}
                        onclick={cycle_theme}
                    >
                        { icons::moon_sun(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::font_change).replace("{font}", store.font_face.as_str())}
                        title={t(lang, Key::font_face)}
                        onclick={cycle_font_face}
                    >
                        { icons::type_face(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::text_size_change).replace("{size}", store.font_scale.as_str())}
                        title={t(lang, Key::text_size)}
                        onclick={cycle_font_scale}
                    >
                        { icons::type_size(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::nav_knowledge)}
                        title={t(lang, Key::nav_knowledge)}
                        onclick={go(Route::Knowledge, p.on_navigate.clone())}
                    >
                        { icons::book(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::nav_publish)}
                        title={t(lang, Key::nav_publish)}
                        onclick={go(Route::Publish, p.on_navigate.clone())}
                    >
                        { icons::globe(18) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet"
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
                        class="topcoat-icon-button--quiet"
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

            <nav class="fn-bottomnav" aria-label={t(lang, Key::nav_sections)}>
                { nav_item(&p.route, "rooms", t(lang, Key::nav_rooms), icons::chat(20), Some(unread),
                           false, Route::Rooms, &p.on_navigate) }
                { nav_item(&p.route, "chat", t(lang, Key::nav_chat), icons::chat(20), None,
                           room.is_none(),
                           room.clone().map(Route::Room).unwrap_or(Route::Rooms), &p.on_navigate) }
                { nav_item(&p.route, "members", t(lang, Key::nav_members), icons::people(20), None,
                           room.is_none(),
                           room.clone().map(Route::Members).unwrap_or(Route::Rooms), &p.on_navigate) }
                { nav_item(&p.route, "invites", t(lang, Key::nav_invites), icons::envelope(20), Some(invites),
                           false, Route::Invitations, &p.on_navigate) }
                { nav_item(&p.route, "knowledge", t(lang, Key::nav_knowledge), icons::book(20), None,
                           false, Route::Knowledge, &p.on_navigate) }
                { nav_item(&p.route, "publish", t(lang, Key::nav_publish), icons::globe(20), None,
                           false, Route::Publish, &p.on_navigate) }
                { nav_item(&p.route, "settings", t(lang, Key::nav_settings), icons::gear(20), None,
                           false, Route::Settings, &p.on_navigate) }
            </nav>
        </div>
    }
}

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
