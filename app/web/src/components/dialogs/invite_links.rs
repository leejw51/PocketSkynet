//! Invite links (ROADMAP §7 M1): mint a link, show it as URL + QR, revoke it.
//!
//! One dialog for both halves of the admin's job — handing a door out and
//! keeping the ledger of doors still open — because they are one decision
//! seen twice: every row in the list below is a link somebody could present
//! right now, and the revoke button is what makes creating one reversible.
//!
//! The token appears exactly once, in the create response this dialog turns
//! into a URL and a QR code. It is not recoverable afterwards — the server
//! stores only a hash — which is why the freshly minted link stays on screen
//! until the dialog closes rather than collapsing into the list.

use pocketskynet_core::RoomId;
use qrcode::render::svg;
use qrcode::QrCode;
use web_sys::HtmlSelectElement;
use yew::prelude::*;

use crate::api::invites::InviteLink;
use crate::format;
use crate::state::{use_store, Load};

use super::super::common::{copy_with_toast, BusyButton, Empty, Skeleton};
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key};

/// Invite links: create, share, revoke. Admin-only, server-enforced.
#[derive(Properties, PartialEq)]
pub struct InviteLinksProps {
    pub room_id: RoomId,
    pub on_close: Callback<()>,
}

/// The QR, as inline SVG. Black-on-white deliberately, in both themes: an
/// inverted QR (light modules on a dark page) scans on some cameras and not
/// others, and a share dialog is the wrong place to discover which kind the
/// newcomer's phone is.
fn qr_svg(url: &str) -> Html {
    match QrCode::new(url.as_bytes()) {
        Ok(code) => {
            let markup = code
                .render::<svg::Color<'_>>()
                .min_dimensions(160, 160)
                .quiet_zone(true)
                .dark_color(svg::Color("#000"))
                .light_color(svg::Color("#fff"))
                .build();
            Html::from_html_unchecked(AttrValue::from(markup))
        }
        // A URL long enough to overflow a QR would be a bug elsewhere; the
        // link line above this is still there, so degrade to nothing.
        Err(_) => html! {},
    }
}

#[function_component(InviteLinks)]
pub fn invite_links(p: &InviteLinksProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let links = use_state(Vec::<InviteLink>::new);
    let load = use_state(Load::default);
    let creating = use_state(|| false);
    let revoking = use_state(|| Option::<String>::None);
    // The link just minted, as the full shareable URL.
    let fresh = use_state(|| Option::<String>::None);
    let expiry_hours = use_state(|| 24i64 * 7);
    let max_uses = use_state(|| Option::<i64>::None);

    let refresh = {
        let store = store.clone();
        let links = links.clone();
        let load = load.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |_: ()| {
            let store = store.clone();
            let links = links.clone();
            let load = load.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.invite_links(&room_id).await {
                    Ok(v) => {
                        links.set(v);
                        load.set(Load::Ready);
                    }
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
        })
    };

    {
        let refresh = refresh.clone();
        let load = load.clone();
        use_effect_with((), move |_| {
            load.set(Load::Loading);
            refresh.emit(());
            || ()
        });
    }

    let create = {
        let store = store.clone();
        let creating = creating.clone();
        let fresh = fresh.clone();
        let expiry_hours = expiry_hours.clone();
        let max_uses = max_uses.clone();
        let refresh = refresh.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |_: MouseEvent| {
            creating.set(true);
            let store = store.clone();
            let creating = creating.clone();
            let fresh = fresh.clone();
            let refresh = refresh.clone();
            let room_id = room_id.clone();
            let hours = *expiry_hours;
            let limit = *max_uses;
            wasm_bindgen_futures::spawn_local(async move {
                match store
                    .client
                    .create_invite_link(&room_id, Some(hours), limit)
                    .await
                {
                    Ok(created) => {
                        fresh.set(Some(
                            store.shareable_url(&format!("/invite/{}", created.token)),
                        ));
                        refresh.emit(());
                    }
                    Err(e) => toast::error(&store, e.user_message(), None),
                }
                creating.set(false);
            });
        })
    };

    let revoke = {
        let store = store.clone();
        let revoking = revoking.clone();
        let refresh = refresh.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |invite_id: String| {
            revoking.set(Some(invite_id.clone()));
            let store = store.clone();
            let revoking = revoking.clone();
            let refresh = refresh.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.revoke_invite_link(&room_id, &invite_id).await {
                    Ok(()) => {
                        // Neutral, not emerald: emerald means "encryption
                        // held", never generic success.
                        toast::neutral(&store, t(lang, Key::invite_link_revoked));
                        refresh.emit(());
                    }
                    Err(e) => toast::error(&store, e.user_message(), None),
                }
                revoking.set(None);
            });
        })
    };

    let close = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: ()| on_close.emit(()))
    };
    let close_click = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: MouseEvent| on_close.emit(()))
    };
    let tz = format::tz_offset_minutes();

    html! {
        <Dialog
            title={t(lang, Key::invite_links)}
            busy={*creating}
            on_close={close}
            footer={Some(html! {
                <>
                    <button type="button" class="topcoat-button" onclick={close_click}>
                        { t(lang, Key::done) }
                    </button>
                    <BusyButton label={t(lang, Key::create_invite_link)} busy={*creating} onclick={create} />
                </>
            })}
        >
            <div class="fn-field">
                <label class="fn-field__label" for="invite-expiry">{ t(lang, Key::invite_expiry_label) }</label>
                <select
                    id="invite-expiry"
                    class="topcoat-select"
                    onchange={{
                        let expiry_hours = expiry_hours.clone();
                        Callback::from(move |e: Event| {
                            if let Some(el) = e.target_dyn_into::<HtmlSelectElement>() {
                                if let Ok(h) = el.value().parse::<i64>() {
                                    expiry_hours.set(h);
                                }
                            }
                        })
                    }}
                >
                    <option value="24" selected={*expiry_hours == 24}>{ t(lang, Key::invite_expiry_1d) }</option>
                    <option value="168" selected={*expiry_hours == 168}>{ t(lang, Key::invite_expiry_7d) }</option>
                    <option value="720" selected={*expiry_hours == 720}>{ t(lang, Key::invite_expiry_30d) }</option>
                </select>
            </div>
            <div class="fn-field">
                <label class="fn-field__label" for="invite-limit">{ t(lang, Key::invite_max_uses_label) }</label>
                <select
                    id="invite-limit"
                    class="topcoat-select"
                    onchange={{
                        let max_uses = max_uses.clone();
                        Callback::from(move |e: Event| {
                            if let Some(el) = e.target_dyn_into::<HtmlSelectElement>() {
                                max_uses.set(el.value().parse::<i64>().ok());
                            }
                        })
                    }}
                >
                    <option value="" selected={max_uses.is_none()}>{ t(lang, Key::invite_no_limit) }</option>
                    <option value="1" selected={*max_uses == Some(1)}>{ "1" }</option>
                    <option value="10" selected={*max_uses == Some(10)}>{ "10" }</option>
                    <option value="25" selected={*max_uses == Some(25)}>{ "25" }</option>
                </select>
            </div>

            if let Some(url) = (*fresh).clone() {
                <div class="fn-field" style="text-align: center;">
                    // White padding behind the QR in both themes — part of the
                    // quiet zone a camera needs, not decoration.
                    <div style="display: inline-block; padding: 8px; background: #fff; border-radius: 8px; line-height: 0;">
                        { qr_svg(&url) }
                    </div>
                    <p style="word-break: break-all; user-select: all;">{ url.clone() }</p>
                    <button
                        type="button"
                        class="topcoat-button--cta"
                        onclick={{
                            let store = store.clone();
                            Callback::from(move |_: MouseEvent| {
                                copy_with_toast(&store, &url, t(lang, Key::invite_link_copied));
                            })
                        }}
                    >{ t(lang, Key::copy_link) }</button>
                </div>
            }

            { match (&*load, links.is_empty()) {
                (Load::Loading, _) => html! { <Skeleton rows={2} /> },
                (Load::Error(e), _) => html! {
                    <Empty art="⚠️" title={t(lang, Key::invite_links)}
                           description={e.clone()} is_error=true />
                },
                (_, true) => html! {
                    <Empty art="🔗" title={t(lang, Key::no_invite_links)}
                           description={""} />
                },
                _ => html! {
                    <div class="fn-picklist">
                        { for links.iter().enumerate().map(|(i, link)| {
                            let acting = revoking.as_ref() == Some(&link.id);
                            let expiry = link.expires_at.as_str();
                            let when = format::parse_iso8601_ms(expiry)
                                .map(|ms| format::short_date(ms, tz))
                                .unwrap_or_else(|| expiry.to_owned());
                            let uses = match link.max_uses {
                                Some(max) => format!("{}/{max}", link.use_count),
                                None => link.use_count.to_string(),
                            };
                            html! {
                                <div key={link.id.clone()} class="fn-picklist__row" style={format!("--i: {i}")}>
                                    <div class="fn-grow">
                                        <div>
                                            if link.expired {
                                                { t(lang, Key::link_expired) }
                                            } else {
                                                { t(lang, Key::invite_expires).replace("{date}", &when) }
                                            }
                                        </div>
                                        <div class="fn-muted">
                                            { t(lang, Key::invite_uses).replace("{count}", &uses) }
                                        </div>
                                    </div>
                                    <BusyButton
                                        label={t(lang, Key::revoke_link)}
                                        class="topcoat-button"
                                        busy={acting}
                                        disabled={revoking.is_some()}
                                        onclick={{
                                            let revoke = revoke.clone();
                                            let id = link.id.clone();
                                            Callback::from(move |_: MouseEvent| revoke.emit(id.clone()))
                                        }}
                                    />
                                </div>
                            }
                        }) }
                    </div>
                },
            } }
        </Dialog>
    }
}
