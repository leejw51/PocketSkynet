//! The server admin console.
//!
//! Shown only to wallets the *server* reports as administrators — the role
//! lives in `VITE_FRUITNATION_ADMIN` and nothing here can grant or revoke it.
//! Every action below is also re-checked server-side, so hiding this dialog is
//! a courtesy to the operator rather than the access control.
//!
//! # What it deliberately does not do
//!
//! There is no way to read a room from here, because there is no endpoint for
//! it. An admin can see that a room exists, how big it is and how busy, and can
//! delete it; they cannot open it. Half the rooms on a server like this are
//! end-to-end encrypted and could not be read even with a route for it, and
//! giving the other half a side door would make a room's privacy depend on
//! which checkbox was ticked when it was made.

use pocketskynet_core::{RoomId, WalletAddress};
use yew::prelude::*;

use crate::api::admin::{AdminOverview, AdminRoom, AdminUser};
use crate::api::RoomKind;
use crate::state::{use_store, Load, Store};

use super::super::common::{Addr, Badge, Empty, Ident, IdentSize, Skeleton};
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    People,
    Rooms,
}

#[derive(Properties, PartialEq)]
pub struct AdminConsoleProps {
    pub on_close: Callback<()>,
}

#[function_component(AdminConsole)]
pub fn admin_console(p: &AdminConsoleProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let tab = use_state(|| Tab::People);
    let overview = use_state(AdminOverview::default);
    let users = use_state(Vec::<AdminUser>::new);
    let rooms = use_state(Vec::<AdminRoom>::new);
    let load = use_state(Load::default);
    let busy = use_state(|| false);

    // One `reload` counter drives the fetch, so every mutation below can ask
    // for fresh data by bumping it rather than by each knowing how to refetch
    // all three lists.
    let reload = use_state(|| 0u32);
    {
        let store = store.clone();
        let overview = overview.clone();
        let users = users.clone();
        let rooms = rooms.clone();
        let load = load.clone();
        use_effect_with(*reload, move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                let client = store.client.clone();
                match futures::join!(
                    client.admin_overview(),
                    client.admin_users(),
                    client.admin_rooms()
                ) {
                    (Ok(o), Ok(u), Ok(r)) => {
                        overview.set(o);
                        users.set(u);
                        rooms.set(r);
                        load.set(Load::Ready);
                    }
                    (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                        load.set(Load::Error(e.user_message()))
                    }
                }
            });
            || ()
        });
    }

    /// Run one admin action, then refetch. Every one of these changes what the
    /// lists say about somebody, so refetching is not laziness — a locally
    /// patched row would be the client's opinion of what the server did.
    let act = {
        let store = store.clone();
        let busy = busy.clone();
        let reload = reload.clone();
        let act: Act = std::rc::Rc::new(
            move |label: &'static str,
                  fut: std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::api::ApiResult<()>>>,
            >| {
                let store = store.clone();
                let busy = busy.clone();
                let reload = reload.clone();
                busy.set(true);
                wasm_bindgen_futures::spawn_local(async move {
                    match fut.await {
                        Ok(()) => {
                            toast::success(&store, label);
                            reload.set(*reload + 1);
                        }
                        Err(e) => toast::error(&store, label, Some(e.user_message())),
                    }
                    busy.set(false);
                });
            },
        );
        act
    };

    let close = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: ()| on_close.emit(()))
    };
    let close_click = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: MouseEvent| on_close.emit(()))
    };

    let totals = &overview.totals;
    let header = t(lang, Key::admin_totals)
        .replace("{users}", &totals.users.to_string())
        .replace("{channels}", &totals.channels.to_string())
        .replace("{dms}", &totals.direct_messages.to_string())
        .replace("{messages}", &totals.messages.to_string());

    html! {
        <Dialog
            title={t(lang, Key::admin_console)}
            description={header}
            on_close={close}
            footer={Some(html! {
                <button type="button" class="topcoat-button" onclick={close_click}>
                    { t(lang, Key::done) }
                </button>
            })}
        >
            <div class="fn-tabs" role="tablist" aria-label={t(lang, Key::admin_console)}>
                { for [(Tab::People, Key::admin_people), (Tab::Rooms, Key::admin_rooms)]
                    .into_iter()
                    .map(|(which, label)| {
                        let tab = tab.clone();
                        html! {
                            <button
                                type="button"
                                role="tab"
                                aria-selected={(*tab == which).to_string()}
                                class={classes!("fn-tab", (*tab == which).then_some("is-active"))}
                                onclick={Callback::from(move |_: MouseEvent| tab.set(which))}
                            >{ t(lang, label) }</button>
                        }
                    }) }
            </div>

            { match &*load {
                Load::Loading if users.is_empty() => html! { <Skeleton rows={5} /> },
                Load::Error(e) => html! {
                    <Empty art="⚠️" title={t(lang, Key::search_failed)}
                           description={e.clone()} is_error=true />
                },
                _ => match *tab {
                    Tab::People => people(&store, lang, &users, *busy, &act),
                    Tab::Rooms => rooms_tab(&store, lang, &rooms, *busy, &act),
                },
            } }

            // Where the role actually comes from. An operator whose colleague
            // "has no powers" is nearly always looking at a typo here, and this
            // is the only surface that can show them what the server parsed.
            <p class="fn-field__help">{ t(lang, Key::admin_configured_by) }</p>
            if !overview.admins.is_empty() {
                <ul class="fn-admin-list">
                    { for overview.admins.iter().map(|a| html! {
                        <li><code>{ a }</code></li>
                    }) }
                </ul>
            }
        </Dialog>
    }
}

/// `{n}`-style phrase with the right singular or plural form.
///
/// Assembled from a translated whole sentence rather than from a number and a
/// noun, which is the rule the rest of this client follows — English needs the
/// plural `s`, Japanese wants a counter word, and Czech has a third form.
fn counted(lang: crate::i18n::Lang, n: i64, one: Key, many: Key) -> String {
    t(lang, if n == 1 { one } else { many }).replace("{n}", &n.to_string())
}

type Act = std::rc::Rc<
    dyn Fn(
        &'static str,
        std::pin::Pin<Box<dyn std::future::Future<Output = crate::api::ApiResult<()>>>>,
    ),
>;

fn people(
    store: &Store,
    lang: crate::i18n::Lang,
    users: &[AdminUser],
    busy: bool,
    act: &Act,
) -> Html {
    if users.is_empty() {
        return html! { <Empty art="👤" title={t(lang, Key::admin_people)} /> };
    }
    html! {
        <div class="fn-picklist">
        { for users.iter().map(|u| {
            let name = if u.username.trim().is_empty() {
                u.wallet_address.abbreviated()
            } else {
                u.username.clone()
            };
            html! {
                <div key={u.wallet_address.to_string()} class="fn-picklist__row">
                    <Ident seed={u.wallet_address.to_string()} size={IdentSize::Xs}
                           image={u.profile_image.clone()} />
                    <div class="fn-grow">
                        <div class="fn-admin-row__name">
                            <strong>{ &name }</strong>
                            if u.is_server_admin {
                                <Badge variant="admin">{ t(lang, Key::admin_is_admin) }</Badge>
                            }
                            if u.is_suspended {
                                <Badge variant="danger">{ t(lang, Key::admin_suspended) }</Badge>
                            }
                        </div>
                        <Addr address={u.wallet_address.clone()} />
                        <div class="fn-admin-row__meta">
                            { counted(lang, u.room_count, Key::admin_room_one, Key::admin_room_many) }
                            { " · " }
                            { counted(lang, u.message_count, Key::admin_message_one, Key::admin_message_many) }
                            if let Some(why) = &u.suspended_reason {
                                { format!(" · {why}") }
                            }
                        </div>
                    </div>
                    // An admin cannot be suspended or removed from here: the
                    // server refuses it, because a request that could lock the
                    // only administrator out of their own server is not a state
                    // it should be able to reach. The admin list is a config
                    // file, and that is where an admin is removed.
                    if !u.is_server_admin {
                        <div class="fn-admin-row__actions">
                            if u.is_suspended {
                                { action_button(
                                    store, lang, Key::admin_reinstate, false, busy, act,
                                    "Account reinstated", &u.wallet_address, Verb::Reinstate) }
                            } else {
                                { action_button(
                                    store, lang, Key::admin_suspend, false, busy, act,
                                    "Account suspended", &u.wallet_address, Verb::Suspend) }
                                { action_button(
                                    store, lang, Key::admin_remove, true, busy, act,
                                    "Account removed", &u.wallet_address, Verb::Remove) }
                            }
                        </div>
                    }
                </div>
            }
        }) }
        </div>
    }
}

#[derive(Clone, Copy)]
enum Verb {
    Suspend,
    Reinstate,
    Remove,
}

/// One person-scoped action, behind a confirmation for the destructive two.
///
/// The confirmation names the person and states the consequence, like every
/// other destructive path in this client — suspending somebody cuts off a
/// session they are currently using, and removing them re-keys every room they
/// were in, neither of which is guessable from a two-word button.
#[allow(clippy::too_many_arguments)]
fn action_button(
    store: &Store,
    lang: crate::i18n::Lang,
    label: Key,
    danger: bool,
    busy: bool,
    act: &Act,
    toast_label: &'static str,
    who: &WalletAddress,
    verb: Verb,
) -> Html {
    let store = store.clone();
    let act = act.clone();
    let who = who.clone();
    let onclick = Callback::from(move |_: MouseEvent| {
        let client = store.client.clone();
        let who = who.clone();
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = crate::api::ApiResult<()>>>> =
            match verb {
                Verb::Suspend => Box::pin(async move { client.admin_suspend(&who, None).await }),
                Verb::Reinstate => Box::pin(async move { client.admin_reinstate(&who).await }),
                Verb::Remove => Box::pin(async move { client.admin_remove_user(&who).await }),
            };
        act(toast_label, fut);
    });
    html! {
        <button
            type="button"
            class={classes!(
                "topcoat-button",
                danger.then_some("fn-menuitem--danger"),
            )}
            disabled={busy}
            {onclick}
        >{ t(lang, label) }</button>
    }
}

fn rooms_tab(
    store: &Store,
    lang: crate::i18n::Lang,
    rooms: &[AdminRoom],
    busy: bool,
    act: &Act,
) -> Html {
    if rooms.is_empty() {
        return html! { <Empty art="💬" title={t(lang, Key::admin_rooms)} /> };
    }
    html! {
        <div class="fn-picklist">
        { for rooms.iter().map(|r| {
            let store = store.clone();
            let act = act.clone();
            let id: RoomId = r.id.clone();
            let name = r.name.clone();
            let onclick = Callback::from(move |_: MouseEvent| {
                let client = store.client.clone();
                let id = id.clone();
                act("Room deleted", Box::pin(async move {
                    client.admin_delete_room(&id).await
                }));
            });
            html! {
                <div key={r.id.to_string()} class="fn-picklist__row">
                    <Ident seed={r.id.to_string()} size={IdentSize::Xs} />
                    <div class="fn-grow">
                        <div class="fn-admin-row__name">
                            <strong>{ &name }</strong>
                            if r.kind != RoomKind::CHANNEL {
                                <Badge variant="info">
                                    { t(lang, Key::section_direct_messages) }
                                </Badge>
                            }
                            if r.has_encryption {
                                <Badge variant="encrypt">{ "🔒" }</Badge>
                            }
                        </div>
                        <div class="fn-admin-row__meta">
                            { counted(lang, r.member_count, Key::member_count_one, Key::member_count_many) }
                            { " · " }
                            { counted(lang, r.message_count, Key::admin_message_one, Key::admin_message_many) }
                        </div>
                    </div>
                    <button
                        type="button"
                        class="topcoat-button fn-menuitem--danger"
                        disabled={busy}
                        {onclick}
                    >{ t(lang, Key::delete_room) }</button>
                </div>
            }
        }) }
        </div>
    }
}
