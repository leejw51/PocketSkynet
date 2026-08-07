//! The invite-link landing page (ROADMAP §7 M1): `/invite/{token}`.
//!
//! The whole funnel in one screen. Signed out, it peeks at the token —
//! unauthenticated by design, the visitor has no account yet — shows what the
//! link opens, parks the token in `localStorage`, and hands over to the login
//! screen; the hook in `app.rs` redeems the parked token the moment a session
//! exists, so the journey reads *link → create wallet → signed in → in the
//! room* with the token surviving every reload in between. Signed in, it
//! skips the ceremony and redeems immediately.
//!
//! A dead link is said plainly, with a door into the app — the visitor may
//! well have a session and just followed something stale from an old chat.

use yew::prelude::*;

use crate::actions;
use crate::api::invites::InvitePeek;
use crate::route::Route;
use crate::session;
use crate::state::{use_store, Load};

use super::common::Skeleton;
use crate::i18n::{t, Key};

/// The invite-link landing page. The one screen reachable without a session
/// besides `/login` itself.
#[derive(Properties, PartialEq)]
pub struct InviteLandingProps {
    /// The bearer token from the URL, already shape-checked by the router.
    pub token: String,
    pub on_navigate: Callback<Route>,
}

#[function_component(InviteLanding)]
pub fn invite_landing(p: &InviteLandingProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let peek = use_state(|| Option::<InvitePeek>::None);
    let load = use_state(Load::default);
    let authenticated = store.auth.is_authenticated();

    {
        // Signed in: redeem now and be in the room; the page is just a
        // spinner on the way past. Signed out: park the token first —
        // *before* the peek resolves, so an impatient tap on "sign in"
        // cannot outrun it — then ask the server what the link opens.
        let store = store.clone();
        let peek = peek.clone();
        let load = load.clone();
        let token = p.token.clone();
        let on_navigate = p.on_navigate.clone();
        use_effect_with((token.clone(), authenticated), move |_| {
            if authenticated {
                wasm_bindgen_futures::spawn_local(actions::redeem_invite(
                    store.clone(),
                    token,
                    on_navigate,
                ));
            } else {
                session::remember_pending_invite(&token);
                load.set(Load::Loading);
                wasm_bindgen_futures::spawn_local(async move {
                    match store.client.peek_invite(&token).await {
                        Ok(v) => {
                            peek.set(Some(v));
                            load.set(Load::Ready);
                        }
                        Err(e) => load.set(Load::Error(e.user_message())),
                    }
                });
            }
            || ()
        });
    }

    if authenticated {
        // Redeeming; nothing to decide here. The toast and the navigation
        // arrive from `actions::redeem_invite`.
        return html! {
            <main class="fn-404">
                <p class="fn-empty__desc">{ t(lang, Key::invite_checking) }</p>
            </main>
        };
    }

    let go_login = {
        let on_navigate = p.on_navigate.clone();
        Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Login))
    };

    html! {
        <main class="fn-404">
            { match &*load {
                Load::Loading => html! { <Skeleton rows={3} /> },
                Load::Error(_) => html! {
                    <>
                        <div class="fn-empty__art" aria-hidden="true">{ "🔗" }</div>
                        <h1 class="fn-empty__title">{ t(lang, Key::invite_invalid) }</h1>
                        <button type="button" class="topcoat-button--cta" onclick={go_login.clone()}>
                            { t(lang, Key::invite_open_app) }
                        </button>
                    </>
                },
                _ => {
                    let (room_line, member_line) = peek
                        .as_ref()
                        .map(|v| (
                            t(lang, Key::invite_room_line).replace("{name}", &v.room_name),
                            t(lang, Key::invite_member_count)
                                .replace("{count}", &v.member_count.to_string()),
                        ))
                        .unwrap_or_default();
                    html! {
                        <>
                            <div class="fn-empty__art" aria-hidden="true">{ "✉️" }</div>
                            <h1 class="fn-empty__title">{ t(lang, Key::youre_invited) }</h1>
                            <p class="fn-empty__desc">{ room_line }</p>
                            <p class="fn-muted">{ member_line }</p>
                            // The login screen carries the rest of the journey:
                            // create a wallet or unlock one, and the parked
                            // token turns the arrival into the room itself.
                            <button type="button" class="topcoat-button--cta" onclick={go_login}>
                                { t(lang, Key::invite_sign_in) }
                            </button>
                        </>
                    }
                }
            } }
        </main>
    }
}
