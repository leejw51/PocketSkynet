//! The More sheet: the sections and tools a phone's bottom nav has no slot for.
//!
//! A tab bar holds five things before the labels start truncating — this app
//! has nine destinations and three tools. The bottom nav keeps the four that
//! are used constantly (rooms, this conversation, its members, the operator's
//! file) and spends its fifth slot on the door to everything else, rather than
//! showing eleven cramped tabs or, as it did, pushing the rest into a top bar
//! that overflowed the screen edge.
//!
//! Rows with words, not a grid of glyphs. This is the surface people arrive at
//! precisely because they could not find something, so it names each thing
//! plainly; the icons are recognition aids beside the labels, not the label.
//!
//! Above 800px none of this is reachable, and nothing here is the only way to
//! anything: the two-pane tier shows every one of these as a top-bar button.

use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::route::Route;
use crate::state::{use_store, Action, Modal};

use super::super::common::Unread;
use super::super::icons;
use super::super::modal::Modal as Dialog;

#[derive(Properties, PartialEq)]
pub struct MoreProps {
    /// Where the app is now, so the row you are already on says so.
    pub route: Route,
    pub on_close: Callback<()>,
    /// Navigates *and* dismisses the sheet — see `render_modal` in app.rs.
    pub on_navigate: Callback<Route>,
}

#[function_component(MoreSheet)]
pub fn more_sheet(p: &MoreProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let invites = store.pending_invitations();

    let go = |route: Route, on_navigate: &Callback<Route>| {
        let on_navigate = on_navigate.clone();
        Callback::from(move |_: MouseEvent| on_navigate.emit(route.clone()))
    };
    // Opening another dialog replaces this one — there is only ever one modal,
    // so the sheet does not have to close itself first.
    let open = |modal: Modal, store: &crate::state::Store| {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| store.dispatch(Action::OpenModal(modal.clone())))
    };

    let current = p.route.nav_key();

    html! {
        <Dialog
            title={t(lang, Key::nav_more).to_owned()}
            description={t(lang, Key::more_hint).to_owned()}
            on_close={p.on_close.clone()}
        >
            <div class="fn-more">
                <section class="fn-more__group" aria-label={t(lang, Key::nav_sections)}>
                    <h3 class="topcoat-list__header">{ t(lang, Key::nav_sections) }</h3>
                    { row(icons::envelope(20), t(lang, Key::invitations), current == "invites",
                          Some(invites), go(Route::Invitations, &p.on_navigate)) }
                    { row(icons::book(20), t(lang, Key::nav_knowledge), current == "knowledge",
                          None, go(Route::Knowledge, &p.on_navigate)) }
                    { row(icons::globe(20), t(lang, Key::nav_publish), current == "publish",
                          None, go(Route::Publish, &p.on_navigate)) }
                    { row(icons::bank(20), t(lang, Key::menu_bank), current == "bank",
                          None, go(Route::Bank, &p.on_navigate)) }
                    { row(icons::lock(20), t(lang, Key::nav_passwords), current == "passwords",
                          None, go(Route::Passwords, &p.on_navigate)) }
                    // Admin-only, exactly as the top bar offers it: hiding it
                    // is a courtesy, the access control is server-side.
                    if store.is_server_admin {
                        { row(icons::gauge(20), t(lang, Key::dash_title), current == "dashboard",
                              None, go(Route::Dashboard, &p.on_navigate)) }
                    }
                    { row(icons::gear(20), t(lang, Key::nav_settings), current == "settings",
                          None, go(Route::Settings, &p.on_navigate)) }
                </section>

                <section class="fn-more__group" aria-label={t(lang, Key::more_tools)}>
                    <h3 class="topcoat-list__header">{ t(lang, Key::more_tools) }</h3>
                    { row(icons::wallet(20), t(lang, Key::wallet), false, None,
                          open(Modal::Wallet, &store)) }
                    { row(icons::megaphone(20), t(lang, Key::shout_title), false, None,
                          open(Modal::Shout, &store)) }
                    { row(icons::server(20), t(lang, Key::server_info), false, None,
                          open(Modal::ServerInfo, &store)) }
                </section>
            </div>
        </Dialog>
    }
}

/// One row: icon, label, and the badge when there is something waiting.
fn row(
    icon: Html,
    label: &'static str,
    current: bool,
    badge: Option<u32>,
    onclick: Callback<MouseEvent>,
) -> Html {
    html! {
        <button
            type="button"
            class="fn-more__row"
            aria-current={current.then_some("page")}
            {onclick}
        >
            { icon }
            <span>{ label }</span>
            if let Some(n) = badge.filter(|n| *n > 0) {
                <Unread count={n} />
            }
        </button>
    }
}
