//! The profile-image picker (Settings → Profile).
//!
//! Three paths to a face, in the order people actually take them: pick one of
//! the twenty gallery portraits (half human, half terminator — the human half
//! carries a characteristic, a coder's glasses, a chef's toque…), have an AI
//! paint a custom one from a one-line description, or upload an image.
//!
//! All three end at the same place: `PUT /api/auth/profile` with either
//! `preset:<slug>` or an `/api/images/…` URL this server hosts. AI and upload
//! bytes go through `POST /api/images` first, so nothing a profile stores can
//! ever point at a foreign origin (`identity::avatar_src` enforces the same
//! rule again at render time).

use yew::prelude::*;

use crate::ai;
use crate::i18n::{t, Key};
use crate::identity;
use crate::state::{use_store, Action, Store};

use super::common::{BusyButton, Spinner};
use super::icons;
use super::toast;

/// The uploads the server's image host accepts (`routes::images::ALLOWED`).
const ACCEPT: &str = "image/png,image/jpeg,image/webp,image/gif";

/// Mirrors `server MAX_IMAGE_BYTES`: refusing here saves a doomed upload.
const MAX_UPLOAD_BYTES: f64 = 5.0 * 1024.0 * 1024.0;

/// The same style spine `tools/genart.py` uses for the shipped gallery, so a
/// generated avatar sits next to the presets as one family rather than a
/// stranger among them. The user's words fill the human half.
fn avatar_prompt(human_half: &str) -> String {
    format!(
        "Close-up portrait, face fills the frame. One continuous face: most \
         of it living human skin, but across one side the skin is torn away \
         in a ragged organic edge, revealing chrome endoskeleton machinery \
         beneath with a calm glowing cyan optic. The torn boundary is \
         irregular and natural — never a straight vertical line — with the \
         metal seamlessly integrated under the skin. The human side is \
         {human_half}. Calm, not menacing. Head-on framing. Ultra realistic \
         cinematic render, Terminator-film industrial machine design, dark \
         background, dramatic rim lighting, tight square crop with the \
         subject filling the frame, photorealistic, no text, no letters, \
         no watermark."
    )
}

/// `PUT` the new value and fold the server's answer into the session.
///
/// `value` is what the wire wants: a `preset:<slug>`, an `/api/images/…`
/// URL, or `""` to clear back to the hash-derived tile.
async fn save(store: &Store, username: &str, value: &str) -> bool {
    let lang = store.language;
    match store.client.update_profile(username, Some(value)).await {
        Ok(user) => {
            store.dispatch(Action::ProfileUpdated(user));
            toast::success(store, t(lang, Key::avatar_updated));
            true
        }
        Err(e) => {
            toast::error(
                store,
                t(lang, Key::avatar_update_failed),
                Some(e.to_string()),
            );
            false
        }
    }
}

#[function_component(AvatarPicker)]
pub fn avatar_picker() -> Html {
    let store = use_store();
    let lang = store.language;
    let offline = !store.online;

    let gallery_open = use_state(|| false);
    let ai_open = use_state(|| false);
    let ai_prompt = use_state(String::new);
    let busy = use_state(|| false);
    let file_input = use_node_ref();

    let Some(username) = store.auth.username().map(str::to_owned) else {
        return html! {};
    };
    let current = store.auth.profile_image().map(str::to_owned);

    // --- pick a gallery portrait -------------------------------------------
    let pick = {
        let store = store.clone();
        let username = username.clone();
        let busy = busy.clone();
        let gallery_open = gallery_open.clone();
        Callback::from(move |slug: &'static str| {
            if *busy {
                return;
            }
            busy.set(true);
            let store = store.clone();
            let username = username.clone();
            let busy = busy.clone();
            let gallery_open = gallery_open.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if save(&store, &username, &format!("preset:{slug}")).await {
                    gallery_open.set(false);
                }
                busy.set(false);
            });
        })
    };

    // --- clear back to the hash tile ---------------------------------------
    let clear = {
        let store = store.clone();
        let username = username.clone();
        let busy = busy.clone();
        Callback::from(move |_: MouseEvent| {
            if *busy {
                return;
            }
            busy.set(true);
            let store = store.clone();
            let username = username.clone();
            let busy = busy.clone();
            wasm_bindgen_futures::spawn_local(async move {
                save(&store, &username, "").await;
                busy.set(false);
            });
        })
    };

    // --- generate with the user's own AI key -------------------------------
    let generate = {
        let store = store.clone();
        let username = username.clone();
        let busy = busy.clone();
        let ai_open = ai_open.clone();
        let ai_prompt = ai_prompt.clone();
        Callback::from(move |_: MouseEvent| {
            if *busy {
                return;
            }
            let settings = ai::AiSettings::load();
            let Some(provider) = settings.image_provider() else {
                toast::error(&store, t(store.language, Key::avatar_need_ai_key), None);
                return;
            };
            let Some(key) = settings.key_for(provider).map(str::to_owned) else {
                toast::error(&store, t(store.language, Key::avatar_need_ai_key), None);
                return;
            };
            let described = ai_prompt.trim().to_owned();
            let prompt = avatar_prompt(if described.is_empty() {
                "an ordinary person"
            } else {
                &described
            });

            busy.set(true);
            let store = store.clone();
            let username = username.clone();
            let busy = busy.clone();
            let ai_open = ai_open.clone();
            let ai_prompt = ai_prompt.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let lang = store.language;
                // Whatever the provider answered with, the profile stores a
                // permanent same-origin URL — the only shape it may store.
                // Bytes are uploaded; a provider link is fetched by the
                // server, because that link is on a foreign origin and
                // expires within about a day.
                let hosted = match ai::generate_image(provider, &key, &prompt).await {
                    Ok(out) => ai::host_generation(&store.client, out).await,
                    Err(e) => Err(e),
                };
                match hosted {
                    Ok(url) => {
                        if save(&store, &username, &url).await {
                            ai_open.set(false);
                            ai_prompt.set(String::new());
                        }
                    }
                    Err(e) => toast::error(&store, t(lang, Key::avatar_update_failed), Some(e)),
                }
                busy.set(false);
            });
        })
    };

    // --- upload -------------------------------------------------------------
    let onpick_file = {
        let store = store.clone();
        let username = username.clone();
        let busy = busy.clone();
        let file_input = file_input.clone();
        Callback::from(move |_: Event| {
            let Some(input) = file_input.cast::<web_sys::HtmlInputElement>() else {
                return;
            };
            let Some(file) = input.files().and_then(|list| list.get(0)) else {
                return;
            };
            // Clear now, so picking the same file twice still fires `change`.
            input.set_value("");
            if *busy {
                return;
            }
            let mime = file.type_();
            if !ACCEPT.split(',').any(|m| m == mime) || file.size() > MAX_UPLOAD_BYTES {
                toast::error(&store, t(store.language, Key::avatar_update_failed), None);
                return;
            }

            busy.set(true);
            let store = store.clone();
            let username = username.clone();
            let busy = busy.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let lang = store.language;
                let blob: web_sys::Blob = file.into();
                let Ok(buffer) = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await
                else {
                    busy.set(false);
                    return;
                };
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                match store.client.upload_image(&mime, bytes).await {
                    Ok(url) => {
                        save(&store, &username, &url).await;
                    }
                    Err(e) => {
                        toast::error(
                            &store,
                            t(lang, Key::avatar_update_failed),
                            Some(e.to_string()),
                        );
                    }
                }
                busy.set(false);
            });
        })
    };

    html! {
        <div class="fn-avatar fn-stack">
            <div class="fn-row fn-row--wrap">
                <button
                    type="button"
                    class="topcoat-button"
                    disabled={offline || *busy}
                    aria-expanded={gallery_open.to_string()}
                    onclick={{
                        let gallery_open = gallery_open.clone();
                        let ai_open = ai_open.clone();
                        Callback::from(move |_: MouseEvent| {
                            ai_open.set(false);
                            gallery_open.set(!*gallery_open);
                        })
                    }}
                >{ t(lang, Key::avatar_pick) }</button>
                <button
                    type="button"
                    class="topcoat-button"
                    disabled={offline || *busy}
                    aria-expanded={ai_open.to_string()}
                    onclick={{
                        let gallery_open = gallery_open.clone();
                        let ai_open = ai_open.clone();
                        Callback::from(move |_: MouseEvent| {
                            gallery_open.set(false);
                            ai_open.set(!*ai_open);
                        })
                    }}
                >{ icons::spark(16) }{ " " }{ t(lang, Key::avatar_make_ai) }</button>
                <button
                    type="button"
                    class="topcoat-button"
                    disabled={offline || *busy}
                    onclick={{
                        let file_input = file_input.clone();
                        Callback::from(move |_: MouseEvent| {
                            if let Some(el) = file_input.cast::<web_sys::HtmlInputElement>() {
                                el.click();
                            }
                        })
                    }}
                >{ t(lang, Key::avatar_upload) }</button>
                if current.is_some() {
                    <button
                        type="button"
                        class="topcoat-button--quiet"
                        disabled={offline || *busy}
                        onclick={clear}
                    >{ t(lang, Key::avatar_default) }</button>
                }
                if *busy {
                    <Spinner />
                }
            </div>
            <input
                ref={file_input.clone()}
                type="file"
                accept={ACCEPT}
                class="fn-sr-only"
                tabindex="-1"
                aria-hidden="true"
                onchange={onpick_file}
            />

            if *gallery_open {
                <div class="fn-avatar__grid" role="listbox" aria-label={t(lang, Key::avatar_pick)}>
                    { for identity::PROFILE_ART.iter().map(|slug| {
                        let value = format!("preset:{slug}");
                        let selected = current.as_deref() == Some(value.as_str());
                        let label = identity::profile_art_label(slug);
                        let pick = pick.clone();
                        html! {
                            <button
                                key={*slug}
                                type="button"
                                role="option"
                                class="fn-avatar__option"
                                aria-selected={selected.to_string()}
                                title={label.clone()}
                                disabled={*busy}
                                onclick={Callback::from(move |_: MouseEvent| pick.emit(slug))}
                            >
                                <img
                                    src={format!("/static/img/{slug}.png")}
                                    alt={label}
                                    loading="lazy"
                                    decoding="async"
                                />
                            </button>
                        }
                    }) }
                </div>
            }

            if *ai_open {
                <div class="fn-stack">
                    <p class="fn-field__help">{ t(lang, Key::avatar_ai_hint) }</p>
                    <div class="fn-row">
                        <input
                            type="text"
                            class="topcoat-text-input fn-grow"
                            placeholder={t(lang, Key::avatar_ai_placeholder)}
                            aria-label={t(lang, Key::avatar_ai_hint)}
                            disabled={*busy}
                            value={(*ai_prompt).clone()}
                            oninput={{
                                let ai_prompt = ai_prompt.clone();
                                Callback::from(move |e: InputEvent| {
                                    use wasm_bindgen::JsCast;
                                    if let Some(el) = e.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) {
                                        ai_prompt.set(el.value());
                                    }
                                })
                            }}
                        />
                        <BusyButton
                            label={t(lang, Key::generate).to_string()}
                            busy={*busy}
                            disabled={offline}
                            onclick={generate}
                        />
                    </div>
                </div>
            }
        </div>
    }
}
