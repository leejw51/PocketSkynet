//! The Files drawer: a room's attachments, with the tag rail that makes them
//! worth having (`docs/API.md` §14).
//!
//! Two design decisions worth stating, because both look like extra work:
//!
//! * **Previews are fetched, not linked.** `/api/files/{id}/raw` needs a bearer
//!   token, so an `<img src>` pointing at it would 401. Thumbnails are fetched
//!   with the token and turned into object URLs, which is also why this
//!   component owns a revoke list — a blob URL leaks its bytes for the lifetime
//!   of the document otherwise.
//! * **Only real image extensions are previewed.** Never `.svg` (it carries
//!   script) and never `.html`. `FileMeta::is_previewable_image` is the gate,
//!   and it reads the extension rather than the declared mime because the
//!   server stores everything as octet-stream — the mime is not evidence.

use std::collections::HashMap;

use pocketskynet_core::RoomId;
use yew::prelude::*;

use crate::api::FileMeta;
use crate::i18n::{t, Key};
use crate::state::{use_store, Load};

use super::super::common::{object_url, save_as, Empty};
use super::super::icons;
use super::super::modal::Modal as Dialog;
use super::super::toast;

/// How many previews one drawer will fetch. Each one costs its whole file, so
/// this is a bandwidth ceiling, not a rendering one.
const MAX_PREVIEWS: usize = 12;

#[derive(Properties, PartialEq)]
pub struct FilesProps {
    pub room_id: RoomId,
    pub on_close: Callback<()>,
}

#[function_component(Files)]
pub fn files(p: &FilesProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let items = use_state(Vec::<FileMeta>::new);
    let load = use_state(Load::default);
    let filter = use_state(|| Option::<String>::None);
    let error = use_state(|| Option::<String>::None);
    // id → object URL for the thumbnails fetched so far.
    let thumbs = use_state(HashMap::<String, String>::new);

    // Load the listing, and reload when the tag filter changes.
    {
        let store = store.clone();
        let items = items.clone();
        let load = load.clone();
        let room_id = p.room_id.clone();
        let tag = (*filter).clone();
        use_effect_with((room_id.clone(), tag.clone()), move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match store
                    .client
                    .list_files(room_id.as_str(), tag.as_deref())
                    .await
                {
                    Ok(list) => {
                        items.set(list);
                        load.set(Load::Ready);
                    }
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
            || ()
        });
    }

    // Fetch previews for the images *and videos* in the listing, once each.
    //
    // A video preview costs the whole file, because there is no Range support
    // anywhere in this server and therefore no way to fetch only the first
    // frame. That is why this is bounded below rather than run over everything.
    {
        let store = store.clone();
        let thumbs = thumbs.clone();
        let list = (*items).clone();
        use_effect_with(
            list.iter().map(|f| f.id.clone()).collect::<Vec<_>>(),
            move |_| {
                let wanted: Vec<FileMeta> = list
                    .iter()
                    .filter(|f| {
                        (f.is_previewable_image() || f.is_previewable_video())
                            && !thumbs.contains_key(&f.id)
                    })
                    // A drawer of forty videos would otherwise download all forty
                    // to draw forty 42px squares. The rest keep their type plate,
                    // which is a perfectly good identifier.
                    .take(MAX_PREVIEWS)
                    .cloned()
                    .collect();
                if !wanted.is_empty() {
                    wasm_bindgen_futures::spawn_local(async move {
                        let mut next = (*thumbs).clone();
                        for f in wanted {
                            if let Ok(bytes) = store.client.download_file(&f.id).await {
                                if let Some(url) = object_url(&bytes, f.preview_mime()) {
                                    next.insert(f.id.clone(), url);
                                }
                            }
                        }
                        thumbs.set(next);
                    });
                }
                || ()
            },
        );
    }

    // Revoke every object URL on unmount. Without this each open/close cycle
    // pins another copy of every thumbnail for the life of the document.
    {
        let thumbs = thumbs.clone();
        use_effect_with((), move |_| {
            move || {
                for url in thumbs.values() {
                    let _ = web_sys::Url::revoke_object_url(url);
                }
            }
        });
    }

    // The tag rail is derived from what is on screen rather than fetched: the
    // listing already carries every file's tags, so a second round trip to
    // /api/search/tags would only be able to disagree with it.
    let all_tags = {
        let mut seen: Vec<String> = Vec::new();
        for f in items.iter() {
            for tag in &f.tags {
                if !seen.contains(tag) {
                    seen.push(tag.clone());
                }
            }
        }
        seen.sort();
        seen
    };

    let save = {
        let store = store.clone();
        Callback::from(move |f: FileMeta| {
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.download_file(&f.id).await {
                    // A save is a fresh blob URL revoked immediately after the
                    // click: the anchor has already read it by then, and holding
                    // it would pin the bytes for nothing.
                    Ok(bytes) => match object_url(&bytes, "application/octet-stream") {
                        Some(url) => {
                            save_as(&url, &f.filename);
                            let _ = web_sys::Url::revoke_object_url(&url);
                        }
                        None => {
                            toast::error(&store, t(store.language, Key::attach_read_failed), None)
                        }
                    },
                    Err(e) => toast::error(&store, e.user_message(), None),
                }
            });
        })
    };

    let remove = {
        let store = store.clone();
        let items = items.clone();
        let error = error.clone();
        Callback::from(move |f: FileMeta| {
            let store = store.clone();
            let items = items.clone();
            let error = error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.delete_file(&f.id).await {
                    Ok(()) => {
                        let kept: Vec<FileMeta> =
                            items.iter().filter(|x| x.id != f.id).cloned().collect();
                        items.set(kept);
                        toast::success(&store, t(store.language, Key::file_deleted));
                    }
                    Err(e) => error.set(Some(e.user_message())),
                }
            });
        })
    };

    let close = p.on_close.clone();

    html! {
        <Dialog
            title={t(lang, Key::files_title)}
            description={t(lang, Key::files_desc).to_owned()}
            on_close={close}
            wide=true
        >
            if !all_tags.is_empty() {
                <div class="fn-files__rail" role="group" aria-label={t(lang, Key::files_title)}>
                    <button
                        type="button"
                        class="fn-files__tag"
                        aria-pressed={(filter.is_none()).to_string()}
                        onclick={{
                            let filter = filter.clone();
                            Callback::from(move |_: MouseEvent| filter.set(None))
                        }}
                    >{ t(lang, Key::file_filter_all) }</button>
                    { for all_tags.iter().map(|tag| {
                        let selected = filter.as_deref() == Some(tag.as_str());
                        let set = {
                            let filter = filter.clone();
                            let tag = tag.clone();
                            Callback::from(move |_: MouseEvent| {
                                // Clicking the active tag clears it, so the rail
                                // never becomes a trap with no way back to All.
                                filter.set((!selected).then(|| tag.clone()))
                            })
                        };
                        html! {
                            <button
                                type="button"
                                class="fn-files__tag"
                                aria-pressed={selected.to_string()}
                                onclick={set}
                            >{ format!("#{tag}") }</button>
                        }
                    }) }
                </div>
            }

            if let Some(e) = &*error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }

            {
                match (&*load, items.is_empty()) {
                    (Load::Error(e), _) => html! {
                        <Empty art="⚠" title={e.clone()} is_error=true />
                    },
                    (Load::Loading, true) => html! { <super::super::common::Skeleton rows={3} /> },
                    (_, true) if filter.is_some() => html! {
                        <Empty
                            art="🏷"
                            title={t(lang, Key::files_none_tagged).to_owned()}
                        />
                    },
                    (_, true) => html! {
                        <Empty
                            art="📎"
                            art_class={classes!("fn-art", "fn-art--files")}
                            title={t(lang, Key::files_empty).to_owned()}
                            description={t(lang, Key::files_empty_desc).to_owned()}
                        />
                    },
                    (_, false) => html! {
                        <ul class="fn-files">
                            { for items.iter().enumerate().map(|(i, f)| html! {
                                <FileRow
                                    key={f.id.clone()}
                                    file={f.clone()}
                                    index={i}
                                    thumb={thumbs.get(&f.id).cloned()}
                                    can_delete={can_delete(&store, f)}
                                    on_save={save.clone()}
                                    on_delete={remove.clone()}
                                />
                            }) }
                        </ul>
                    },
                }
            }
        </Dialog>
    }
}

#[derive(Properties, PartialEq)]
struct FileRowProps {
    file: FileMeta,
    index: usize,
    thumb: Option<String>,
    can_delete: bool,
    on_save: Callback<FileMeta>,
    on_delete: Callback<FileMeta>,
}

#[function_component(FileRow)]
fn file_row(p: &FileRowProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let f = &p.file;
    let ext = f.extension();

    html! {
        <li class="fn-file" style={format!("--i: {}", p.index)}>
            <span class="fn-file__plate" aria-hidden="true">
                if let Some(url) = &p.thumb {
                    if f.is_previewable_video() {
                        // Muted, controlless and never playing: this is a poster
                        // frame, and `preload="metadata"` is what makes the
                        // browser decode one for us without a canvas dance.
                        <video
                            class="fn-file__thumb"
                            src={url.clone()}
                            muted=true
                            preload="metadata"
                        />
                    } else {
                        <img class="fn-file__thumb" src={url.clone()} alt="" />
                    }
                } else if ext.is_empty() {
                    { icons::files(18) }
                } else {
                    <span class="fn-file__ext">{ ext.to_uppercase() }</span>
                }
            </span>

            <span class="fn-file__body">
                <span class="fn-file__name fn-truncate" title={f.filename.clone()}>
                    { &f.filename }
                </span>
                <span class="fn-file__meta fn-nums">
                    { f.human_size() }
                    { " · " }
                    { relative(&f.created_at) }
                </span>
                if !f.tags.is_empty() {
                    <span class="fn-file__tags">
                        { for f.tags.iter().map(|tag| html! {
                            <span class="fn-file__tag">{ format!("#{tag}") }</span>
                        }) }
                    </span>
                }
            </span>

            <span class="fn-file__tools">
                <button
                    type="button"
                    class="topcoat-icon-button--quiet"
                    aria-label={t(lang, Key::file_download)}
                    title={t(lang, Key::file_download)}
                    onclick={{
                        let cb = p.on_save.clone();
                        let f = f.clone();
                        Callback::from(move |_: MouseEvent| cb.emit(f.clone()))
                    }}
                >{ icons::download(16) }</button>
                if p.can_delete {
                    <button
                        type="button"
                        class="topcoat-icon-button--quiet fn-file__danger"
                        aria-label={t(lang, Key::file_delete)}
                        title={t(lang, Key::file_delete)}
                        onclick={{
                            let cb = p.on_delete.clone();
                            let f = f.clone();
                            Callback::from(move |_: MouseEvent| cb.emit(f.clone()))
                        }}
                    >{ icons::trash(16) }</button>
                }
            </span>
        </li>
    }
}

/// The uploader, or an admin of the room it lives in — mirroring the server's
/// rule in `routes/files.rs::remove`. Shown optimistically: the button being
/// absent is a UI courtesy, and the server is what actually enforces this.
fn can_delete(store: &crate::state::Store, f: &FileMeta) -> bool {
    let mine = store.me().is_some_and(|me| me == &f.uploader);
    let admin = store
        .room(&f.room_id)
        .zip(store.me())
        .is_some_and(|(room, me)| room.is_admin(me));
    mine || admin
}

/// `3 minutes ago`, or nothing when the server sent something unparseable —
/// a broken timestamp must not blank the whole row.
fn relative(created_at: &str) -> String {
    match crate::format::parse_iso8601_ms(created_at) {
        Some(ms) => crate::format::relative_time(ms, crate::format::now_ms()),
        None => String::new(),
    }
}
