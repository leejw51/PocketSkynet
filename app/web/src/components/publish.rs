//! Web publishing — the paid hosting wall (docs/API.md §16.2).
//!
//! Pay the publish price and this server hosts your page at `/sites/{id}/`:
//! a pasted HTML document, an uploaded `.html` file, or a zip carrying
//! `index.html` plus assets. The page lists every hosted site newest-first,
//! filters locally, and every card carries a **Remove** button for *any*
//! signed-in user — the wall is shared, and the community prunes it. Deep
//! search runs through the Knowledge page: every site is indexed globally
//! (kind `site`).
//!
//! The payment flow mirrors the Shout dialog: pay once through
//! [`super::bank::pay_operator`], keep the hash if the publish call fails,
//! never pay twice for one page.

use web_sys::{HtmlInputElement, HtmlTextAreaElement};
use yew::prelude::*;

use crate::api::sites::Site;
use crate::format;
use crate::i18n::{t, Key};
use crate::route::Route;
use crate::state::use_store;

use super::common::{Back, Empty};
use super::icons;
use super::toast;

const MAX_TITLE_CHARS: usize = 100;

/// The page's own origin (`https://100.120.4.113:9777`), so the URL a card
/// shows is one this viewer can hand to someone else verbatim — over
/// Tailscale, a LAN address, or localhost, whichever way *they* reached the
/// server. Derived from `location` like the WebSocket URL, never compiled in.
#[cfg(target_arch = "wasm32")]
fn page_origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn page_origin() -> String {
    String::new()
}

/// The shareable absolute URL of a site.
fn full_url(origin: &str, site_url: &str) -> String {
    format!("{}{}", origin.trim_end_matches('/'), site_url)
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Paste,
    Upload,
}

#[derive(Properties, PartialEq)]
pub struct PublishProps {
    pub on_navigate: Callback<Route>,
}

#[function_component(Publish)]
pub fn publish(p: &PublishProps) -> Html {
    let store = use_store();
    let lang = store.language;

    let sites = use_state(Vec::<Site>::new);
    // The server-recommended base for shareable URLs (Tailscale first, then
    // LAN). `None` until the list loads, or when the server is loopback-only
    // — then the viewer's own origin is the only address there is.
    let share_base = use_state(|| Option::<String>::None);
    let loading = use_state(|| true);
    let filter = use_state(String::new);

    let mode = use_state(|| Mode::Paste);
    let title = use_state(String::new);
    let pasted = use_state(String::new);
    let picked = use_state(|| Option::<(String, Vec<u8>)>::None);
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let paid_tx = use_state(|| Option::<String>::None);
    // The site id whose Remove button is armed, waiting for the second,
    // deliberate click.
    let arming = use_state(|| Option::<String>::None);

    let file_input = use_node_ref();

    let price = {
        let configured = store.chain.publish_price_cro.trim();
        if configured.is_empty() {
            "1".to_owned()
        } else {
            configured.to_owned()
        }
    };
    let symbol = store
        .active_network()
        .map(|n| n.symbol.clone())
        .unwrap_or_else(|| "CRO".to_owned());
    let price_label = format!("{price} {symbol}");

    let reload = {
        let store = store.clone();
        let sites = sites.clone();
        let share_base = share_base.clone();
        let loading = loading.clone();
        Callback::from(move |_: ()| {
            let store = store.clone();
            let sites = sites.clone();
            let share_base = share_base.clone();
            let loading = loading.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.sites().await {
                    Ok(listing) => {
                        sites.set(listing.sites);
                        share_base.set(listing.share_base);
                    }
                    Err(e) => toast::error(&store, "Publish", Some(e.user_message())),
                }
                loading.set(false);
            });
        })
    };

    {
        let reload = reload.clone();
        use_effect_with((), move |_| {
            reload.emit(());
            || ()
        });
    }

    let onpick = {
        let picked = picked.clone();
        let file_input = file_input.clone();
        Callback::from(move |_: Event| {
            let Some(input) = file_input.cast::<HtmlInputElement>() else {
                return;
            };
            let Some(file) = input.files().and_then(|list| list.get(0)) else {
                return;
            };
            let name = file.name();
            // Clear now, so re-picking the same file still fires `change`.
            input.set_value("");
            let picked = picked.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let blob: web_sys::Blob = file.into();
                let Ok(buffer) = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await
                else {
                    return;
                };
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                picked.set(Some((name, bytes)));
            });
        })
    };

    let run = {
        let store = store.clone();
        let mode = mode.clone();
        let title = title.clone();
        let pasted = pasted.clone();
        let picked = picked.clone();
        let busy = busy.clone();
        let error = error.clone();
        let paid_tx = paid_tx.clone();
        let reload = reload.clone();
        let price = price.clone();
        Callback::from(move |_: ()| {
            if *busy {
                return;
            }
            let page_title = title.trim().to_owned();
            if page_title.is_empty() || page_title.chars().count() > MAX_TITLE_CHARS {
                error.set(Some(t(lang, Key::publish_title_invalid).to_owned()));
                return;
            }
            let bytes: Vec<u8> = match *mode {
                Mode::Paste => pasted.trim().as_bytes().to_vec(),
                Mode::Upload => picked.as_ref().map(|(_, b)| b.clone()).unwrap_or_default(),
            };
            if bytes.is_empty() {
                error.set(Some(t(lang, Key::publish_need_content).to_owned()));
                return;
            }
            busy.set(true);
            error.set(None);

            let store = store.clone();
            let title = title.clone();
            let pasted = pasted.clone();
            let picked = picked.clone();
            let busy = busy.clone();
            let error = error.clone();
            let paid_tx = paid_tx.clone();
            let reload = reload.clone();
            let price = price.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = async {
                    let tx_hash = match paid_tx.as_ref() {
                        Some(hash) => hash.clone(),
                        None => {
                            let hash = super::bank::pay_operator(&store, &price).await?;
                            paid_tx.set(Some(hash.clone()));
                            hash
                        }
                    };
                    store
                        .client
                        .publish_site(&page_title, &tx_hash, bytes)
                        .await
                        .map_err(|e| e.user_message())
                }
                .await;

                match outcome {
                    Ok(site) => {
                        toast::success(&store, t(lang, Key::publish_sent));
                        title.set(String::new());
                        pasted.set(String::new());
                        picked.set(None);
                        paid_tx.set(None);
                        let _ = site;
                        reload.emit(());
                    }
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        })
    };

    let copy_url = {
        let store = store.clone();
        Callback::from(move |url: String| {
            let store = store.clone();
            super::common::copy_then(&url, move |ok| {
                if ok {
                    toast::success(&store, t(store.language, Key::publish_url_copied));
                } else {
                    // No clipboard on this platform — the URL is on screen; say
                    // so rather than pretending.
                    toast::warn(&store, t(store.language, Key::publish_copy_failed), None);
                }
            });
        })
    };

    let remove = {
        let store = store.clone();
        let sites = sites.clone();
        let arming = arming.clone();
        Callback::from(move |id: String| {
            // First click arms; only the second, on the same card, fires.
            if arming.as_ref() != Some(&id) {
                arming.set(Some(id));
                return;
            }
            arming.set(None);
            let store = store.clone();
            let sites = sites.clone();
            let id_for_filter = id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.delete_site(&id).await {
                    Ok(()) => {
                        toast::neutral(&store, t(store.language, Key::publish_removed));
                        sites.set(
                            sites
                                .iter()
                                .filter(|s| s.id != id_for_filter)
                                .cloned()
                                .collect(),
                        );
                    }
                    Err(e) => toast::error(&store, "Publish", Some(e.user_message())),
                }
            });
        })
    };

    let needle = filter.trim().to_lowercase();
    let visible: Vec<&Site> = sites
        .iter()
        .filter(|s| {
            needle.is_empty()
                || s.title.to_lowercase().contains(&needle)
                || s.username.to_lowercase().contains(&needle)
        })
        .collect();
    let now = format::now_ms();
    let me = store.me().map(|w| w.as_str().to_owned());
    // The server knows which of its addresses works off this machine; the
    // page only knows how *it* got here. Prefer the server's answer.
    let origin = share_base.as_ref().cloned().unwrap_or_else(page_origin);

    let button_label = if paid_tx.is_some() {
        t(lang, Key::publish_retry).to_owned()
    } else {
        t(lang, Key::publish_pay).replace("{price}", &price_label)
    };
    let can_submit = !*busy
        && !title.trim().is_empty()
        && match *mode {
            Mode::Paste => !pasted.trim().is_empty(),
            Mode::Upload => picked.is_some(),
        };

    html! {
        <>
        <div class="topcoat-navigation-bar">
            <Back onclick={{
                let on_navigate = p.on_navigate.clone();
                Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Rooms))
            }} />
            <h1 class="topcoat-navigation-bar__title">{ t(lang, Key::nav_publish) }</h1>
        </div>
        <div class="fn-scroll fn-publish">
            <div class="fn-publish__hero">
                <img src={crate::asset::img(store.skin, "publish-emblem")} alt="" aria-hidden="true" />
                <p class="fn-muted">
                    { t(lang, Key::publish_hint).replace("{price}", &price_label) }
                </p>
            </div>

            <section class="fn-publish__form" aria-label={t(lang, Key::publish_your_page)}>
                <label class="fn-field">
                    <span class="fn-field__label">{ t(lang, Key::publish_form_title) }</span>
                    <input
                        class="topcoat-text-input fn-grow"
                        type="text"
                        maxlength={MAX_TITLE_CHARS.to_string()}
                        placeholder={t(lang, Key::publish_title_placeholder)}
                        value={(*title).clone()}
                        disabled={*busy}
                        oninput={{
                            let title = title.clone();
                            Callback::from(move |e: InputEvent| {
                                let el: HtmlInputElement = e.target_unchecked_into();
                                title.set(el.value());
                            })
                        }}
                    />
                </label>

                <div class="fn-tabs" role="tablist" aria-label={t(lang, Key::publish_your_page)}>
                    { mode_tab(lang, Key::publish_mode_paste, Mode::Paste, &mode) }
                    { mode_tab(lang, Key::publish_mode_upload, Mode::Upload, &mode) }
                </div>

                if *mode == Mode::Paste {
                    <textarea
                        class="topcoat-textarea fn-publish__paste"
                        rows={8}
                        placeholder={t(lang, Key::publish_paste_placeholder)}
                        value={(*pasted).clone()}
                        disabled={*busy}
                        oninput={{
                            let pasted = pasted.clone();
                            Callback::from(move |e: InputEvent| {
                                let el: HtmlTextAreaElement = e.target_unchecked_into();
                                pasted.set(el.value());
                            })
                        }}
                    />
                } else {
                    <div class="fn-publish__pickrow">
                        <button
                            type="button"
                            class="topcoat-button"
                            disabled={*busy}
                            onclick={{
                                let file_input = file_input.clone();
                                Callback::from(move |_: MouseEvent| {
                                    if let Some(input) = file_input.cast::<HtmlInputElement>() {
                                        input.click();
                                    }
                                })
                            }}
                        >
                            { icons::upload(16) }
                            { " " }
                            { t(lang, Key::publish_pick_file) }
                        </button>
                        if let Some((name, bytes)) = picked.as_ref() {
                            <span class="fn-publish__pickname">
                                { t(lang, Key::publish_picked)
                                    .replace("{name}", name)
                                    .replace("{size}", &human_size(bytes.len() as i64)) }
                            </span>
                        }
                        <input
                            ref={file_input.clone()}
                            type="file"
                            accept=".html,.htm,.zip"
                            class="fn-sr-only"
                            onchange={onpick}
                        />
                    </div>
                }

                if paid_tx.is_some() {
                    <p class="fn-publish__paid">{ t(lang, Key::shout_paid_note) }</p>
                }
                if let Some(e) = error.as_ref() {
                    <p class="fn-publish__error" role="alert">{ e.clone() }</p>
                }
                <div class="fn-publish__actions">
                    <button
                        type="button"
                        class="topcoat-button--large--cta"
                        disabled={!can_submit}
                        onclick={{
                            let run = run.clone();
                            Callback::from(move |_: MouseEvent| run.emit(()))
                        }}
                    >
                        { icons::globe(16) }
                        <span>{ button_label }</span>
                    </button>
                </div>
            </section>

            <section class="fn-publish__wall" aria-label={t(lang, Key::nav_publish)}>
                <div class="fn-publish__filterrow">
                    <input
                        class="topcoat-text-input fn-grow"
                        type="search"
                        placeholder={t(lang, Key::publish_filter)}
                        value={(*filter).clone()}
                        oninput={{
                            let filter = filter.clone();
                            Callback::from(move |e: InputEvent| {
                                let el: HtmlInputElement = e.target_unchecked_into();
                                filter.set(el.value());
                            })
                        }}
                    />
                </div>

                if *loading {
                    <div class="fn-publish__loading"><span class="fn-spinner" aria-hidden="true" /></div>
                } else if visible.is_empty() {
                    <Empty
                        art="🌐"
                        art_class={classes!("fn-art--publish")}
                        title={t(lang, Key::publish_empty)}
                        description={t(lang, Key::publish_empty_desc).replace("{price}", &price_label)}
                    />
                } else {
                    <ul class="fn-publish__list">
                        { for visible.iter().map(|s| {
                            site_card(lang, s, &origin, &me, now, &arming, &remove, &copy_url)
                        }) }
                    </ul>
                }
            </section>
        </div>
        </>
    }
}

fn mode_tab(lang: crate::i18n::Lang, key: Key, this: Mode, mode: &UseStateHandle<Mode>) -> Html {
    let selected = **mode == this;
    let onclick = {
        let mode = mode.clone();
        Callback::from(move |_: MouseEvent| mode.set(this))
    };
    html! {
        <button
            type="button"
            class="fn-tab"
            role="tab"
            aria-selected={selected.to_string()}
            {onclick}
        >{ t(lang, key) }</button>
    }
}

#[allow(clippy::too_many_arguments)]
fn site_card(
    lang: crate::i18n::Lang,
    site: &Site,
    origin: &str,
    me: &Option<String>,
    now: i64,
    arming: &UseStateHandle<Option<String>>,
    remove: &Callback<String>,
    copy_url: &Callback<String>,
) -> Html {
    let mine = me.as_deref() == Some(site.owner_address.as_str());
    let armed = arming.as_ref() == Some(&site.id);
    let share_url = full_url(origin, &site.url);
    html! {
        <li class="fn-hitcard fn-sitecard" key={site.id.clone()}>
            <div class="fn-hitcard__meta">
                <span class="fn-hitcard__kind fn-sitecard__kind">
                    { icons::globe(12) }
                    { " " }
                    { t(lang, Key::nav_publish) }
                </span>
                <span class="fn-sitecard__owner">{ &site.username }</span>
                if mine {
                    <span class="fn-sitecard__you">{ t(lang, Key::you) }</span>
                }
                <time class="fn-hitcard__time">{ format::relative_time(site.created_at, now) }</time>
            </div>
            <div class="fn-sitecard__title">{ &site.title }</div>
            <div class="fn-sitecard__meta2">
                { t(lang, Key::publish_meta)
                    .replace("{files}", &site.file_count.to_string())
                    .replace("{size}", &human_size(site.size_bytes)) }
            </div>
            // The full, shareable address — the origin this viewer reached
            // the server on (a Tailscale IP stays a Tailscale IP), so what
            // is on screen is exactly what a copy hands to someone else.
            <code class="fn-sitecard__url" title={share_url.clone()}>{ share_url.clone() }</code>
            <div class="fn-sitecard__actions">
                <a
                    class="topcoat-button fn-sitecard__open"
                    href={site.url.clone()}
                    target="_blank"
                    rel="noopener noreferrer"
                >
                    { icons::external(14) }
                    { " " }
                    { t(lang, Key::publish_open) }
                </a>
                <button
                    type="button"
                    class="topcoat-button fn-sitecard__copy"
                    title={t(lang, Key::publish_copy_url)}
                    onclick={{
                        let copy_url = copy_url.clone();
                        let url = share_url.clone();
                        Callback::from(move |_: MouseEvent| copy_url.emit(url.clone()))
                    }}
                >
                    { icons::copy(14) }
                    { " " }
                    { t(lang, Key::publish_copy_url) }
                </button>
                <button
                    type="button"
                    class={classes!(
                        "topcoat-button--quiet",
                        "fn-sitecard__remove",
                        armed.then_some("fn-sitecard__remove--armed")
                    )}
                    onclick={{
                        let remove = remove.clone();
                        let id = site.id.clone();
                        Callback::from(move |_: MouseEvent| remove.emit(id.clone()))
                    }}
                >
                    { if armed { t(lang, Key::publish_remove_arm) } else { t(lang, Key::publish_remove) } }
                </button>
            </div>
        </li>
    }
}

/// `1234` → `1.2 KB`; sizes are shown to people, not machines.
fn human_size(bytes: i64) -> String {
    let bytes = bytes.max(0) as f64;
    if bytes < 1024.0 {
        format!("{bytes:.0} B")
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1} KB", bytes / 1024.0)
    } else {
        format!("{:.1} MB", bytes / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shareable_url_is_origin_plus_path_with_no_double_slash() {
        assert_eq!(
            full_url("https://100.120.4.113:9777", "/sites/abc123/"),
            "https://100.120.4.113:9777/sites/abc123/"
        );
        assert_eq!(
            full_url("https://100.120.4.113:9777/", "/sites/abc123/"),
            "https://100.120.4.113:9777/sites/abc123/",
            "a trailing slash on the origin must not double up"
        );
        // Host tests have no window; the card degrades to the relative path
        // rather than lying about an address.
        assert_eq!(full_url("", "/sites/abc123/"), "/sites/abc123/");
    }

    #[test]
    fn sizes_read_like_a_person_wrote_them() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(1_500_000), "1.4 MB");
        assert_eq!(
            human_size(-5),
            "0 B",
            "a negative size is a server bug, not a render bug"
        );
    }
}
