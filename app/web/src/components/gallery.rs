//! The room's photo gallery — every picture and clip, as a grid.
//!
//! A room's media already renders in the chat stream, but finding last
//! month's photo by scrolling is archaeology. This screen is the same media
//! rearranged by the question being asked: *what has this room shown me?* —
//! a dense newest-first grid, the shape every phone's photo roll has taught.
//!
//! One request paints it: `GET /api/rooms/{id}/media` returns tiles whose
//! URLs are ready to use — attachment tiles arrive carrying their `?dl=`
//! capability, hosted tiles are capability-by-hash already — so the grid
//! costs a listing plus the thumbnails themselves, never a mint per tile
//! (see the server's `routes/gallery.rs`).
//!
//! Tapping a picture raises the app's lightbox with the **full** image;
//! tapping a video plays it in a full-screen overlay. Videos wear a play
//! badge in the grid, because a grid flattens everything into stills and the
//! badge is what says this one moves.

use pocketskynet_core::RoomId;
use yew::prelude::*;

use crate::api::files::GalleryItem;
use crate::route::Route;
use crate::state::{use_store, Load};

use super::common::{Back, Empty, Skeleton};
use super::icons;
use crate::i18n::{t, Key};

#[derive(Properties, PartialEq)]
pub struct GalleryProps {
    pub room_id: RoomId,
    pub on_navigate: Callback<Route>,
}

/// Page size. Generous because a tile costs a row of metadata and a lazy
/// `<img>`, and "load more" clicks are friction on what is meant to be a
/// scroll.
const PAGE: u32 = 120;

#[function_component(Gallery)]
pub fn gallery(p: &GalleryProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let items = use_state(Vec::<GalleryItem>::new);
    let has_more = use_state(|| false);
    let load = use_state(Load::default);
    // A video being watched, as its playable URL. Full-screen overlay state
    // rather than in-tile playback: a 150px tile is nowhere to watch anything.
    let playing = use_state(|| Option::<String>::None);

    {
        let store = store.clone();
        let items = items.clone();
        let has_more = has_more.clone();
        let load = load.clone();
        let room_id = p.room_id.clone();
        use_effect_with(room_id.clone(), move |_| {
            load.set(Load::Loading);
            items.set(Vec::new());
            wasm_bindgen_futures::spawn_local(async move {
                match store
                    .client
                    .room_media(room_id.as_str(), None, Some(PAGE))
                    .await
                {
                    Ok(page) => {
                        items.set(page.items);
                        has_more.set(page.has_more);
                        load.set(Load::Ready);
                    }
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
            || ()
        });
    }

    let load_more = {
        let store = store.clone();
        let items = items.clone();
        let has_more = has_more.clone();
        let room_id = p.room_id.clone();
        Callback::from(move |_: MouseEvent| {
            // The cursor is the oldest tile on screen; the server answers
            // with what came before it.
            let Some(before) = items.iter().map(|i| i.created_at_ms).reduce(f64::min) else {
                return;
            };
            let store = store.clone();
            let items = items.clone();
            let has_more = has_more.clone();
            let room_id = room_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(page) = store
                    .client
                    .room_media(room_id.as_str(), Some(before), Some(PAGE))
                    .await
                {
                    let mut all = (*items).clone();
                    // The cursor is exclusive, but concurrent posting can
                    // still hand back a tile twice; the key dedupes it.
                    for item in page.items {
                        if !all.iter().any(|i| i.key() == item.key()) {
                            all.push(item);
                        }
                    }
                    items.set(all);
                    has_more.set(page.has_more);
                }
            });
        })
    };

    // Escape puts a playing video away, matching the lightbox.
    {
        let playing = playing.clone();
        use_effect_with(playing.is_some(), move |open| {
            let listener = open.then(|| {
                gloo_events::EventListener::new(
                    &web_sys::window().expect("a browser window"),
                    "keydown",
                    move |e| {
                        use wasm_bindgen::JsCast;
                        if let Some(e) = e.dyn_ref::<web_sys::KeyboardEvent>() {
                            if e.key() == "Escape" {
                                playing.set(None);
                            }
                        }
                    },
                )
            });
            move || drop(listener)
        });
    }

    let grid = match &*load {
        Load::Loading if items.is_empty() => html! { <Skeleton rows={4} /> },
        Load::Error(e) => html! {
            <Empty art="⚠️" title={t(lang, Key::gallery_couldnt_load)}
                   art_class="fn-art--offline" description={e.clone()} />
        },
        _ if items.is_empty() => html! {
            <Empty art="📷" title={t(lang, Key::gallery_empty)}
                   description={t(lang, Key::gallery_empty_desc)} />
        },
        _ => html! {
            <>
                <ul class="fn-gallery" role="list">
                    { for items.iter().map(|item| tile(&store, item, &playing)) }
                </ul>
                if *has_more {
                    <div class="fn-gallery__more">
                        <button type="button" class="topcoat-button" onclick={load_more}>
                            { t(lang, Key::gallery_load_more) }
                        </button>
                    </div>
                }
            </>
        },
    };

    html! {
        <>
            <div class="topcoat-navigation-bar">
                <Back onclick={{
                    let on_navigate = p.on_navigate.clone();
                    let id = p.room_id.clone();
                    Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Room(id.clone())))
                }} />
                <h1 class="topcoat-navigation-bar__title">{ t(lang, Key::gallery_title) }</h1>
                <span class="fn-badge fn-badge--muted">{ items.len() }</span>
            </div>

            <div class="fn-gallery-wrap fn-scroll">
                { grid }
            </div>

            if let Some(src) = (*playing).clone() {
                <div
                    class="fn-gallery__player"
                    role="dialog"
                    aria-modal="true"
                    aria-label={t(lang, Key::video_play)}
                    onclick={{
                        let playing = playing.clone();
                        Callback::from(move |_: MouseEvent| playing.set(None))
                    }}
                >
                    // Clicks on the player are the controls, not a dismiss.
                    <video
                        src={src}
                        controls=true
                        autoplay=true
                        playsinline=true
                        preload="auto"
                        onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}
                    />
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-gallery__player-close"
                        aria-label={t(lang, Key::close)}
                        onclick={{
                            let playing = playing.clone();
                            Callback::from(move |_: MouseEvent| playing.set(None))
                        }}
                    >{ icons::close(20) }</button>
                </div>
            }
        </>
    }
}

/// One tile: the thumbnail when the server holds one, the full image when it
/// does not (pre-thumbnail uploads), a dark plate for a video with no poster.
fn tile(
    store: &crate::state::Store,
    item: &GalleryItem,
    playing: &UseStateHandle<Option<String>>,
) -> Html {
    let lang = store.language;
    // Attachment URLs carry `?dl=` and need `inline=1` for a real media
    // Content-Type; hosted URLs are served with theirs already.
    let full = if item.source == "attachment" {
        store.client.url(&format!("{}&inline=1", item.url))
    } else {
        store.client.url(&item.url)
    };
    let thumb = item.thumb_url.as_ref().map(|u| store.client.url(u));
    let caption = item
        .filename
        .clone()
        .or_else(|| item.caption.clone().filter(|c| !c.is_empty()));

    let img_ref = NodeRef::default();
    let onclick = if item.is_video() {
        let playing = playing.clone();
        let full = full.clone();
        Callback::from(move |_: MouseEvent| playing.set(Some(full.clone())))
    } else {
        let img_ref = img_ref.clone();
        let full = thumb.is_some().then(|| full.clone());
        let caption = caption.clone();
        Callback::from(move |_: MouseEvent| {
            super::message::zoom_past_thumbnail(&img_ref, full.clone(), caption.clone());
        })
    };

    let shot = match (&thumb, item.is_video()) {
        // The one case with nothing to show: a video nobody posted a frame
        // for. A plate rather than a `<video preload="metadata">`, because a
        // grid of a hundred of those is a hundred header fetches — the exact
        // cost this feature exists to remove.
        (None, true) => html! { <span class="fn-gallery__plate" aria-hidden="true"></span> },
        (src, video) => {
            let src = src.clone().unwrap_or_else(|| full.clone());
            let alt = caption.clone().unwrap_or_default();
            html! {
                <img
                    ref={if !video { img_ref.clone() } else { NodeRef::default() }}
                    class="fn-gallery__shot"
                    {src}
                    {alt}
                    loading="lazy"
                />
            }
        }
    };

    html! {
        <li class="fn-gallery__cell" key={item.key()}>
            <button
                type="button"
                class="fn-gallery__tile"
                aria-label={if item.is_video() {
                    t(lang, Key::video_play).to_owned()
                } else {
                    caption.clone().unwrap_or_else(|| t(lang, Key::image_zoom).to_owned())
                }}
                title={caption.unwrap_or_default()}
                {onclick}
            >
                { shot }
                if item.is_video() {
                    <span class="fn-gallery__badge" aria-hidden="true">
                        { icons::play(18) }
                    </span>
                }
            </button>
        </li>
    }
}
