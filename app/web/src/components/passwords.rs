//! Skynet Password — the encrypted key/value store (docs/API.md §18).
//!
//! A list, a form and a generator. The list shows each entry's **label**,
//! decrypted here from a key derived off the session's E2EE identity; the
//! server saw six opaque strings per row and nothing else. The honest account
//! of what that does and does not buy lives in [`crate::secrets`] and in
//! `core/src/secrets.rs` — this module is the part people touch.
//!
//! # Decrypted values are opened late and dropped early
//!
//! The security rule this component is shaped around (see [`crate::secrets`]):
//! a decrypted *value* is never cached, never held in a list, never persisted.
//! So the list is built from **labels only**, and the label pass is memoised so
//! it runs once per fetch rather than once per keystroke. A **value** is opened
//! only at the instant the user reveals or copies one entry, and the plaintext
//! is dropped the moment the reveal is collapsed or the copy completes — it is
//! never stored beside the label. The residue that cannot be removed — a value
//! briefly in the heap while it is on screen, and the plaintext in the input
//! field while it is being typed — is exactly that; everything else is
//! minimized.
//!
//! # Three decisions worth stating
//!
//! **Values are masked until asked for.** A password manager whose list shows
//! every password is a screenshot waiting to happen, and the person most likely
//! to read yours is standing behind you, not attacking your server. Reveal is
//! per entry and single-valued: showing one hides the last.
//!
//! **The generator can refuse.** If the browser has no CSPRNG,
//! [`pocketskynet_core::password::generate`] returns an error and this screen
//! shows it instead of filling the field. That is the whole point: a generator
//! that quietly falls back to something guessable hands you a weak password
//! with no way to find out.
//!
//! **Copy tells the truth.** It goes through [`super::common::copy_then`],
//! which awaits the clipboard promise and falls back to the legacy selection
//! copy on an insecure origin — this app is *meant* to be opened on a plain
//! `http://` LAN address, where `navigator.clipboard` does not exist. A failure
//! says so and points at the reveal button, rather than claiming a copy that
//! never happened.

use pocketskynet_core::password::{self, Recipe, MAX_LENGTH, MIN_LENGTH};
use pocketskynet_core::secrets::{VaultKey, MAX_SECRET_BYTES};
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::api::passwords::PasswordEntry;
use crate::format;
use crate::i18n::{t, Key, Lang};
use crate::route::Route;
use crate::secrets::{sealed_labels, Opened, SealError, SecretLabel, Vault};
use crate::state::use_store;

use super::common::{Back, Empty, Spinner};
use super::icons;
use super::toast;

/// How wide the mask is, whatever the secret's real length.
///
/// Fixed rather than one dot per character: the length of a password is worth
/// something to somebody guessing it, and a row of eight dots beside a row of
/// twenty publishes exactly that to anyone glancing at the screen.
const MASK: &str = "••••••••••";

/// What the form is currently doing.
#[derive(Clone, PartialEq)]
enum Draft {
    /// Nothing open.
    Closed,
    /// Composing a new entry.
    New,
    /// Editing the entry with this id.
    Editing(String),
}

impl Draft {
    fn editing_id(&self) -> Option<&str> {
        match self {
            Draft::Editing(id) => Some(id),
            _ => None,
        }
    }

    fn is_open(&self) -> bool {
        !matches!(self, Draft::Closed)
    }
}

#[derive(Properties, PartialEq)]
pub struct PasswordsProps {
    pub on_navigate: Callback<Route>,
}

#[function_component(Passwords)]
pub fn passwords(p: &PasswordsProps) -> Html {
    let store = use_store();
    let lang = store.language;

    // The rows as the server holds them — ciphertext, and only ciphertext, in
    // state. Opening them into labels happens in the memo below; opening a
    // value happens on demand, in a handler, and is never stored here.
    let rows = use_state(Vec::<PasswordEntry>::new);
    let loading = use_state(|| true);
    let filter = use_state(String::new);

    let draft = use_state(|| Draft::Closed);
    let draft_name = use_state(String::new);
    let draft_secret = use_state(String::new);
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);

    // The one revealed value, decrypted on demand and held only while it is on
    // screen. `(id, opened)` — single-valued, so revealing a second entry drops
    // the first's plaintext, and collapsing sets it back to `None`. This is the
    // *only* place a decrypted value lives, and it lives here transiently.
    let revealed = use_state(|| Option::<(String, Opened)>::None);
    let arming = use_state(|| Option::<String>::None);

    let gen_open = use_state(|| false);
    let recipe = use_state(Recipe::default);

    let reload = {
        let store = store.clone();
        let rows = rows.clone();
        let loading = loading.clone();
        Callback::from(move |_: ()| {
            let store = store.clone();
            let rows = rows.clone();
            let loading = loading.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.passwords().await {
                    Ok(list) => rows.set(list),
                    Err(e) => toast::error(
                        &store,
                        t(store.language, Key::pw_title),
                        Some(e.user_message()),
                    ),
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

    // The vault key for this session, or `None` when locked. Cloned once here
    // rather than re-borrowed per handler — it is two subkeys, held in process
    // memory for the tab's life and never persisted (`crate::session`).
    let vault_key: Option<VaultKey> = store.auth.session().map(|s| s.keys.borrow().vault_key());
    let locked = vault_key.is_none();

    // Decrypt the labels once per fetch, not once per render.
    //
    // Every keystroke in the filter or the form re-renders this component. The
    // label pass is behind `use_memo` keyed on the rows' identity (id +
    // `updated_at`, which an edit advances), the lock state and the signed-in
    // account, so it reruns only when the list actually changes, the session
    // locks/unlocks, or the account changes — not on every character typed.
    // The account has to be part of the key, not just `locked`: if this
    // component ever stayed mounted across an account switch that went
    // straight from one unlocked key to another (no intermediate `locked`
    // frame) and the rows hadn't refetched yet, keying on `locked` alone would
    // miss it and the memo would keep serving labels decrypted with the
    // previous account's key. Values are deliberately *not* in here; see the
    // module docs.
    let fingerprint: Vec<(String, i64)> =
        rows.iter().map(|r| (r.id.clone(), r.updated_at)).collect();
    let account = store.auth.address().cloned();
    let labels = {
        let rows = rows.clone();
        let vault_key = vault_key.clone();
        use_memo((fingerprint, locked, account), move |_| match &vault_key {
            Some(k) => Vault::from_key(k.clone()).labels(&rows),
            None => sealed_labels(&rows),
        })
    };
    let visible: Vec<&SecretLabel> = labels.iter().filter(|l| l.matches(&filter)).collect();

    let close_draft = {
        let draft = draft.clone();
        let draft_name = draft_name.clone();
        let draft_secret = draft_secret.clone();
        let error = error.clone();
        let gen_open = gen_open.clone();
        Callback::from(move |_: ()| {
            draft.set(Draft::Closed);
            draft_name.set(String::new());
            // Clearing the secret on close is not tidiness: a draft left in
            // component state is a plaintext password sitting in the heap for
            // as long as the tab lives.
            draft_secret.set(String::new());
            error.set(None);
            gen_open.set(false);
        })
    };

    let start_new = {
        let draft = draft.clone();
        let draft_name = draft_name.clone();
        let draft_secret = draft_secret.clone();
        let error = error.clone();
        let revealed = revealed.clone();
        Callback::from(move |_: MouseEvent| {
            draft.set(Draft::New);
            draft_name.set(String::new());
            draft_secret.set(String::new());
            error.set(None);
            revealed.set(None);
        })
    };

    let start_edit = {
        let store = store.clone();
        let rows = rows.clone();
        let vault_key = vault_key.clone();
        let draft = draft.clone();
        let draft_name = draft_name.clone();
        let draft_secret = draft_secret.clone();
        let error = error.clone();
        let revealed = revealed.clone();
        Callback::from(move |id: String| {
            let (Some(k), Some(row)) = (vault_key.as_ref(), rows.iter().find(|r| r.id == id))
            else {
                return;
            };
            let vault = Vault::from_key(k.clone());
            // Both halves must be readable to edit into the form. Prefilling an
            // unreadable value with the empty string and saving would replace a
            // secret this session cannot read with nothing at all — so an
            // unreadable half refuses the edit rather than risking that.
            let (Opened::Text(name), Opened::Text(secret)) =
                (vault.open_label(row).key, vault.open_value(row))
            else {
                toast::warn(&store, t(store.language, Key::pw_sealed), None);
                return;
            };
            draft_name.set(name);
            draft_secret.set(secret);
            draft.set(Draft::Editing(row.id.clone()));
            error.set(None);
            revealed.set(None);
        })
    };

    let generate = {
        let draft_secret = draft_secret.clone();
        let error = error.clone();
        let recipe = recipe.clone();
        Callback::from(move |_: MouseEvent| {
            match password::generate(&recipe) {
                Ok(pw) => {
                    draft_secret.set(pw);
                    error.set(None);
                }
                // The refusal path. Nothing is written to the field — a
                // half-generated or fallback password is worse than none,
                // because the person holding it cannot tell.
                Err(password::PasswordError::NoCharacterClasses) => {
                    error.set(Some(t(lang, Key::pw_gen_no_classes).to_owned()))
                }
                Err(password::PasswordError::Randomness) => {
                    error.set(Some(t(lang, Key::pw_gen_failed).to_owned()))
                }
                // Unreachable from this UI — the slider clamps to the valid
                // range — but if it ever fired, the message has to be about
                // length, not about character classes.
                Err(password::PasswordError::Length) => error.set(Some(
                    t(lang, Key::pw_gen_length_bad)
                        .replace("{min}", &MIN_LENGTH.to_string())
                        .replace("{max}", &MAX_LENGTH.to_string()),
                )),
            }
        })
    };

    let save = {
        let store = store.clone();
        let vault_key = vault_key.clone();
        let draft = draft.clone();
        let draft_name = draft_name.clone();
        let draft_secret = draft_secret.clone();
        let busy = busy.clone();
        let error = error.clone();
        let rows = rows.clone();
        let close_draft = close_draft.clone();
        Callback::from(move |_: MouseEvent| {
            if *busy {
                return;
            }
            let name = draft_name.trim().to_owned();
            if name.is_empty() {
                error.set(Some(t(lang, Key::pw_needs_name).to_owned()));
                return;
            }
            let secret = (*draft_secret).clone();
            let editing = draft.editing_id().map(str::to_owned);

            // Sealing needs the session keys, which a locked tab does not have.
            // The form is not shown when locked; this is the backstop.
            let Some(k) = vault_key.as_ref() else {
                error.set(Some(t(lang, Key::pw_locked_desc).to_owned()));
                return;
            };
            let vault = Vault::from_key(k.clone());

            let id = match &editing {
                Some(id) => id.clone(),
                None => match crate::secrets::new_entry_id() {
                    Ok(id) => id,
                    // A dead CSPRNG cannot mint an id either. Same refusal as
                    // the generator's: say so, store nothing.
                    Err(_) => {
                        error.set(Some(t(lang, Key::pw_gen_failed).to_owned()));
                        return;
                    }
                },
            };

            let (sealed_key, sealed_value) = match vault.seal(&id, &name, &secret) {
                Ok(pair) => pair,
                // The size cap has its own message — a 5 KB paste is the user's
                // to shorten, not a broken device.
                Err(SealError::TooLong) => {
                    error.set(Some(
                        t(lang, Key::pw_secret_too_long)
                            .replace("{max}", &MAX_SECRET_BYTES.to_string()),
                    ));
                    return;
                }
                Err(SealError::Failed) => {
                    error.set(Some(t(lang, Key::pw_seal_failed).to_owned()));
                    return;
                }
            };

            busy.set(true);
            error.set(None);
            let store = store.clone();
            let busy = busy.clone();
            let error = error.clone();
            let rows = rows.clone();
            let close_draft = close_draft.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = match &editing {
                    Some(id) => {
                        store
                            .client
                            .update_password(id, &sealed_key, &sealed_value)
                            .await
                    }
                    None => {
                        store
                            .client
                            .create_password(&id, &sealed_key, &sealed_value)
                            .await
                    }
                };
                match outcome {
                    Ok(entry) => {
                        toast::success(
                            &store,
                            t(
                                store.language,
                                if editing.is_some() {
                                    Key::pw_updated
                                } else {
                                    Key::pw_saved
                                },
                            ),
                        );
                        close_draft.emit(());
                        // Merge the row the server just handed back rather than
                        // refetching the whole list: a save is scoped to one
                        // row, the response already carries its authoritative
                        // `updated_at`, and on an account near the entry cap a
                        // refetch would mean a second round trip plus
                        // re-decrypting every other label just to render the
                        // one that changed.
                        let mut next = (*rows).clone();
                        match next.iter().position(|r| r.id == entry.id) {
                            Some(pos) => next[pos] = entry,
                            None => next.push(entry),
                        }
                        next.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
                        rows.set(next);
                    }
                    Err(e) => error.set(Some(e.user_message())),
                }
                busy.set(false);
            });
        })
    };

    let reveal = {
        let rows = rows.clone();
        let vault_key = vault_key.clone();
        let revealed = revealed.clone();
        Callback::from(move |id: String| {
            // Toggle: a second click on the shown entry hides it and drops the
            // plaintext.
            if revealed.as_ref().map(|(i, _)| i.as_str()) == Some(id.as_str()) {
                revealed.set(None);
                return;
            }
            let (Some(k), Some(row)) = (vault_key.as_ref(), rows.iter().find(|r| r.id == id))
            else {
                return;
            };
            // Decrypt here, at the moment of reveal — not earlier, and not for
            // any other row.
            let opened = Vault::from_key(k.clone()).open_value(row);
            revealed.set(Some((id, opened)));
        })
    };

    let copy = {
        let store = store.clone();
        let rows = rows.clone();
        let vault_key = vault_key.clone();
        Callback::from(move |id: String| {
            let (Some(k), Some(row)) = (vault_key.as_ref(), rows.iter().find(|r| r.id == id))
            else {
                return;
            };
            match Vault::from_key(k.clone()).open_value(row) {
                Opened::Text(value) => {
                    let store = store.clone();
                    // `value` moves into the closure and is dropped once the
                    // copy settles — never stored.
                    super::common::copy_then(&value, move |ok| {
                        if ok {
                            toast::success(&store, t(store.language, Key::pw_copied));
                        } else {
                            toast::warn(&store, t(store.language, Key::pw_copy_failed), None);
                        }
                    });
                }
                Opened::Sealed => toast::warn(&store, t(store.language, Key::pw_sealed), None),
            }
        })
    };

    let remove = {
        let store = store.clone();
        let arming = arming.clone();
        let reload = reload.clone();
        Callback::from(move |id: String| {
            // First click arms, second fires — the same two-step the Publish
            // wall uses. A modal for a one-row delete is heavier than the act.
            if arming.as_ref() != Some(&id) {
                arming.set(Some(id));
                return;
            }
            arming.set(None);
            let store = store.clone();
            let reload = reload.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.delete_password(&id).await {
                    Ok(()) => {
                        toast::neutral(&store, t(store.language, Key::pw_removed));
                        // Refetch rather than splice the pre-await snapshot: two
                        // overlapping deletes would each rewrite a stale list
                        // and one could reappear. The authoritative list is one
                        // request away, and the server is already correct.
                        reload.emit(());
                    }
                    Err(e) => toast::error(
                        &store,
                        t(store.language, Key::pw_title),
                        Some(e.user_message()),
                    ),
                }
            });
        })
    };

    let now = format::now_ms();

    html! {
        <>
        <div class="topcoat-navigation-bar">
            <Back onclick={{
                let on_navigate = p.on_navigate.clone();
                Callback::from(move |_: MouseEvent| on_navigate.emit(Route::Rooms))
            }} />
            <h1 class="topcoat-navigation-bar__title">{ t(lang, Key::pw_title) }</h1>
        </div>
        <div class="fn-scroll fn-pw">
            <div class="fn-pw__hero">
                { icons::lock(28) }
                <div>
                    <p class="fn-muted">{ t(lang, Key::pw_hint) }</p>
                    <p class="fn-pw__promise">{ t(lang, Key::pw_only_you) }</p>
                    // Said on the screen, not only in a doc comment: the
                    // recovery story is "there isn't one", and somebody about
                    // to trust this with their bank password deserves to read
                    // that before they do rather than after.
                    <p class="fn-pw__warning">
                        { icons::warn(14) }
                        { " " }
                        { t(lang, Key::pw_lost_warning) }
                    </p>
                </div>
            </div>

            if locked {
                // Locked is a *listing*, not a wall: the rows below still show,
                // sealed, with delete. This banner explains why, and how to
                // turn the seals into text.
                <p class="fn-pw__lockednote">
                    { icons::lock(14) }
                    { " " }
                    { t(lang, Key::pw_locked_desc) }
                </p>
            } else {
                <section class="fn-pw__form" aria-label={t(lang, Key::pw_add)}>
                    if draft.is_open() {
                        { draft_form(
                            lang, &draft, &draft_name, &draft_secret, &recipe, &gen_open,
                            &busy, &error, &generate, &save, &close_draft,
                        ) }
                    } else {
                        <button
                            type="button"
                            class="topcoat-button--large--cta fn-pw__addbtn"
                            onclick={start_new}
                        >
                            { icons::plus(16) }
                            <span>{ t(lang, Key::pw_add) }</span>
                        </button>
                    }
                </section>
            }

            <section class="fn-pw__list" aria-label={t(lang, Key::pw_title)}>
                <div class="fn-pw__filterrow">
                    <input
                        class="topcoat-search-input fn-grow"
                        type="search"
                        placeholder={t(lang, Key::pw_filter)}
                        value={(*filter).clone()}
                        oninput={{
                            let filter = filter.clone();
                            Callback::from(move |e: InputEvent| {
                                let el: HtmlInputElement = e.target_unchecked_into();
                                filter.set(el.value());
                            })
                        }}
                    />
                    <span class="fn-pw__count">
                        { t(lang, Key::pw_count).replace("{n}", &labels.len().to_string()) }
                    </span>
                </div>

                if *loading {
                    <div class="fn-pw__loading"><Spinner /></div>
                } else if visible.is_empty() {
                    <Empty
                        art="🔑"
                        title={t(lang, Key::pw_empty)}
                        description={t(lang, Key::pw_empty_desc)}
                    />
                } else {
                    <ul class="fn-pw__rows">
                        { for visible.iter().map(|label| entry_card(
                            lang, label, now, &revealed, &arming,
                            &remove, &copy, &reveal, &start_edit,
                        )) }
                    </ul>
                }
            </section>
        </div>
        </>
    }
}

/// The add/edit form, including the generator panel.
#[allow(clippy::too_many_arguments)]
fn draft_form(
    lang: Lang,
    draft: &UseStateHandle<Draft>,
    name: &UseStateHandle<String>,
    secret: &UseStateHandle<String>,
    recipe: &UseStateHandle<Recipe>,
    gen_open: &UseStateHandle<bool>,
    busy: &UseStateHandle<bool>,
    error: &UseStateHandle<Option<String>>,
    generate: &Callback<MouseEvent>,
    save: &Callback<MouseEvent>,
    close: &Callback<()>,
) -> Html {
    let editing = draft.editing_id().is_some();
    let can_save = !**busy && !name.trim().is_empty();

    html! {
        <div class="fn-pw__draft">
            <label class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::pw_name_label) }</span>
                <input
                    class="topcoat-text-input fn-grow"
                    type="text"
                    autocomplete="off"
                    maxlength={MAX_SECRET_BYTES.to_string()}
                    placeholder={t(lang, Key::pw_name_placeholder)}
                    value={(**name).clone()}
                    disabled={**busy}
                    oninput={{
                        let name = name.clone();
                        Callback::from(move |e: InputEvent| {
                            let el: HtmlInputElement = e.target_unchecked_into();
                            name.set(el.value());
                        })
                    }}
                />
            </label>

            <label class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::pw_secret_label) }</span>
                <input
                    class="topcoat-text-input fn-grow fn-pw__secretinput"
                    // `type=text`, not `password`: this field is *composing* a
                    // secret the person is about to save, and a masked field
                    // they cannot proof-read is how a typo becomes a password
                    // nobody can reproduce. The stored value is masked in the
                    // list, which is where a bystander would actually read it.
                    type="text"
                    autocomplete="off"
                    spellcheck="false"
                    autocapitalize="off"
                    // A soft cap matching the seal's byte limit. The seal
                    // re-checks bytes (a maxlength counts UTF-16 units), so this
                    // only spares the common case a round trip to a refusal.
                    maxlength={MAX_SECRET_BYTES.to_string()}
                    placeholder={t(lang, Key::pw_secret_placeholder)}
                    value={(**secret).clone()}
                    disabled={**busy}
                    oninput={{
                        let secret = secret.clone();
                        Callback::from(move |e: InputEvent| {
                            let el: HtmlInputElement = e.target_unchecked_into();
                            secret.set(el.value());
                        })
                    }}
                />
            </label>

            <div class="fn-pw__genrow">
                <button
                    type="button"
                    class="topcoat-button fn-pw__gentoggle"
                    aria-expanded={gen_open.to_string()}
                    onclick={{
                        let gen_open = gen_open.clone();
                        Callback::from(move |_: MouseEvent| gen_open.set(!*gen_open))
                    }}
                >
                    { icons::spark(14) }
                    { " " }
                    { t(lang, Key::pw_gen_title) }
                </button>
                <button
                    type="button"
                    class="topcoat-button--cta fn-pw__genbtn"
                    disabled={**busy}
                    onclick={generate.clone()}
                >
                    { t(lang, Key::pw_generate) }
                </button>
                <span class="fn-pw__bits">
                    { t(lang, Key::pw_gen_strength)
                        .replace("{bits}", &entropy_label(recipe)) }
                </span>
            </div>

            if **gen_open {
                <div class="fn-pw__gen">
                    <label class="fn-pw__len">
                        <span>{ t(lang, Key::pw_gen_length) }</span>
                        <input
                            type="range"
                            min={MIN_LENGTH.to_string()}
                            max={MAX_LENGTH.to_string()}
                            value={recipe.length.to_string()}
                            oninput={{
                                let recipe = recipe.clone();
                                Callback::from(move |e: InputEvent| {
                                    let el: HtmlInputElement = e.target_unchecked_into();
                                    let length = el
                                        .value()
                                        .parse::<usize>()
                                        .unwrap_or(password::DEFAULT_LENGTH)
                                        .clamp(MIN_LENGTH, MAX_LENGTH);
                                    recipe.set(Recipe { length, ..*recipe });
                                })
                            }}
                        />
                        <output>{ recipe.length }</output>
                    </label>
                    <div class="fn-pw__classes">
                        { class_toggle(lang, Key::pw_gen_lowercase, recipe, |r| r.lowercase, |r, v| r.lowercase = v) }
                        { class_toggle(lang, Key::pw_gen_uppercase, recipe, |r| r.uppercase, |r, v| r.uppercase = v) }
                        { class_toggle(lang, Key::pw_gen_digits, recipe, |r| r.digits, |r, v| r.digits = v) }
                        { class_toggle(lang, Key::pw_gen_symbols, recipe, |r| r.symbols, |r, v| r.symbols = v) }
                    </div>
                </div>
            }

            if let Some(e) = error.as_ref() {
                <p class="fn-pw__error" role="alert">{ e.clone() }</p>
            }

            <div class="fn-pw__actions">
                <button
                    type="button"
                    class="topcoat-button--cta"
                    disabled={!can_save}
                    onclick={save.clone()}
                >
                    { if editing { t(lang, Key::pw_save) } else { t(lang, Key::pw_add) } }
                </button>
                <button
                    type="button"
                    class="topcoat-button"
                    disabled={**busy}
                    onclick={{
                        let close = close.clone();
                        Callback::from(move |_: MouseEvent| close.emit(()))
                    }}
                >
                    { t(lang, Key::cancel) }
                </button>
            </div>
        </div>
    }
}

/// One character-class switch.
fn class_toggle(
    lang: Lang,
    key: Key,
    recipe: &UseStateHandle<Recipe>,
    get: fn(&Recipe) -> bool,
    set: fn(&mut Recipe, bool),
) -> Html {
    let on = get(recipe);
    let onclick = {
        let recipe = recipe.clone();
        Callback::from(move |_: MouseEvent| {
            let mut next = *recipe;
            let flipped = !get(&next);
            set(&mut next, flipped);
            recipe.set(next);
        })
    };
    html! {
        <button
            type="button"
            class={classes!("fn-pw__class", on.then_some("fn-pw__class--on"))}
            aria-pressed={on.to_string()}
            {onclick}
        >
            { t(lang, key) }
        </button>
    }
}

/// One stored entry, rendered from its label.
///
/// The value is not on the label — it is decrypted on demand into `revealed`
/// when this entry is shown, and read from there. An entry whose label is
/// sealed (a foreign wallet's row, or a locked session) offers only delete.
#[allow(clippy::too_many_arguments)]
fn entry_card(
    lang: Lang,
    label: &SecretLabel,
    now: i64,
    revealed: &UseStateHandle<Option<(String, Opened)>>,
    arming: &UseStateHandle<Option<String>>,
    remove: &Callback<String>,
    copy: &Callback<String>,
    reveal: &Callback<String>,
    start_edit: &Callback<String>,
) -> Html {
    let is_revealed = revealed.as_ref().map(|(i, _)| i.as_str()) == Some(label.id.as_str());
    let revealed_value = is_revealed
        .then(|| revealed.as_ref().map(|(_, o)| o.clone()))
        .flatten();
    let armed = arming.as_ref() == Some(&label.id);
    let readable = label.is_readable();

    html! {
        <li class="fn-hitcard fn-pwcard" key={label.id.clone()}>
            <div class="fn-pwcard__head">
                <span class="fn-pwcard__name">
                    { match &label.key {
                        Opened::Text(k) => html! { { k.clone() } },
                        Opened::Sealed => html! {
                            <em class="fn-pwcard__sealed">{ t(lang, Key::pw_sealed) }</em>
                        },
                    } }
                </span>
                <time class="fn-hitcard__time">
                    { format::relative_time(label.updated_at, now) }
                </time>
            </div>

            <div class="fn-pwcard__secret">
                { match (readable, revealed_value) {
                    // Revealed and decrypted: show it.
                    (true, Some(Opened::Text(v))) => html! {
                        <code class="fn-pwcard__value">{ v }</code>
                    },
                    // Revealed but the value itself is corrupt.
                    (true, Some(Opened::Sealed)) => html! {
                        <em class="fn-pwcard__sealed">{ t(lang, Key::pw_sealed) }</em>
                    },
                    // Readable label, value not revealed: a fixed-width mask.
                    (true, None) => html! {
                        <code class="fn-pwcard__value fn-pwcard__value--masked"
                              aria-label={t(lang, Key::pw_secret_label)}>{ MASK }</code>
                    },
                    // Label sealed (foreign wallet or locked): nothing to show.
                    (false, _) => html! {
                        <em class="fn-pwcard__sealed">{ t(lang, Key::pw_sealed) }</em>
                    },
                } }
            </div>

            <div class="fn-pwcard__actions">
                if readable {
                    <button
                        type="button"
                        class="topcoat-button fn-pwcard__reveal"
                        onclick={{
                            let reveal = reveal.clone();
                            let id = label.id.clone();
                            Callback::from(move |_: MouseEvent| reveal.emit(id.clone()))
                        }}
                    >
                        { if is_revealed { icons::eye_off(14) } else { icons::eye(14) } }
                        { " " }
                        { if is_revealed { t(lang, Key::pw_hide) } else { t(lang, Key::pw_reveal) } }
                    </button>
                    <button
                        type="button"
                        class="topcoat-button fn-pwcard__copy"
                        title={t(lang, Key::pw_copy)}
                        onclick={{
                            let copy = copy.clone();
                            let id = label.id.clone();
                            Callback::from(move |_: MouseEvent| copy.emit(id.clone()))
                        }}
                    >
                        { icons::copy(14) }
                        { " " }
                        { t(lang, Key::pw_copy) }
                    </button>
                    <button
                        type="button"
                        class="topcoat-button fn-pwcard__edit"
                        onclick={{
                            let start_edit = start_edit.clone();
                            let id = label.id.clone();
                            Callback::from(move |_: MouseEvent| start_edit.emit(id.clone()))
                        }}
                    >
                        { t(lang, Key::edit) }
                    </button>
                }
                // Always offered, readable or not: a row this session cannot
                // open is exactly the one somebody most wants to be rid of.
                <button
                    type="button"
                    class={classes!(
                        "topcoat-button--quiet",
                        "fn-pwcard__remove",
                        armed.then_some("fn-pwcard__remove--armed")
                    )}
                    onclick={{
                        let remove = remove.clone();
                        let id = label.id.clone();
                        Callback::from(move |_: MouseEvent| remove.emit(id.clone()))
                    }}
                >
                    { icons::trash(14) }
                    { " " }
                    { if armed { t(lang, Key::pw_remove_arm) } else { t(lang, Key::pw_remove) } }
                </button>
            </div>
        </li>
    }
}

/// The entropy figure the form shows, rounded to a whole bit.
///
/// Rounded down, not to nearest: a meter that rounds 127.6 up to 128 is
/// claiming a round number the recipe did not earn, and 128 is precisely the
/// number people have opinions about.
fn entropy_label(recipe: &Recipe) -> String {
    (recipe.entropy_bits().floor() as i64).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(length: usize) -> Recipe {
        Recipe {
            length,
            ..Recipe::default()
        }
    }

    #[test]
    fn the_mask_is_a_fixed_width_and_says_nothing_about_the_length() {
        // The property, asserted rather than assumed: a ten-character password
        // and a forty-character one must look identical in the list, or the
        // list publishes password lengths to anyone glancing at the screen.
        assert_eq!(MASK.chars().count(), 10);
        assert!(!MASK.is_empty());
    }

    #[test]
    fn the_entropy_meter_rounds_down_rather_than_flattering_the_recipe() {
        // 20 characters of the 94-character alphabet is ~131.08 bits.
        assert_eq!(entropy_label(&recipe(20)), "131");
        // 19 is ~124.5 — it must not present as 125.
        assert_eq!(entropy_label(&recipe(19)), "124");
        // Longer is always at least as strong.
        let a: i64 = entropy_label(&recipe(12)).parse().unwrap();
        let b: i64 = entropy_label(&recipe(32)).parse().unwrap();
        assert!(b > a);
    }

    #[test]
    fn a_recipe_with_nothing_enabled_claims_no_entropy() {
        let empty = Recipe {
            lowercase: false,
            uppercase: false,
            digits: false,
            symbols: false,
            ..Recipe::default()
        };
        assert_eq!(entropy_label(&empty), "0");
    }

    #[test]
    fn the_draft_state_knows_which_entry_it_is_editing() {
        assert!(!Draft::Closed.is_open());
        assert_eq!(Draft::Closed.editing_id(), None);
        assert!(Draft::New.is_open());
        assert_eq!(Draft::New.editing_id(), None, "a new entry has no id yet");
        let editing = Draft::Editing("sec_00112233445566778899aabbccddeeff".into());
        assert!(editing.is_open());
        assert_eq!(
            editing.editing_id(),
            Some("sec_00112233445566778899aabbccddeeff")
        );
    }

    #[test]
    fn every_generator_failure_has_its_own_message() {
        // The refusal path is the point of the generator. Each error must map
        // to copy that tells the user what to do about it — collapsing them
        // into one string is how "your browser has no CSPRNG" ends up reading
        // as "you ticked the wrong box".
        let no_classes = t(Lang::En, Key::pw_gen_no_classes);
        let randomness = t(Lang::En, Key::pw_gen_failed);
        let length = t(Lang::En, Key::pw_gen_length_bad);
        // All three distinct — including Length, which used to alias the
        // "no character classes" string.
        assert_ne!(no_classes, randomness);
        assert_ne!(no_classes, length);
        assert_ne!(randomness, length);
        assert!(length.to_lowercase().contains("length"));
        // And the randomness one must say nothing was generated, because the
        // field staying empty is otherwise indistinguishable from a bug.
        assert!(randomness.to_lowercase().contains("nothing was generated"));
    }

    #[test]
    fn the_oversize_message_names_the_cap_and_is_not_the_device_failure() {
        // Fix #5: a value past the cap is the user's to shorten, and must read
        // differently from a CSPRNG death ("this device could not seal that").
        let too_long =
            t(Lang::En, Key::pw_secret_too_long).replace("{max}", &MAX_SECRET_BYTES.to_string());
        let device = t(Lang::En, Key::pw_seal_failed);
        assert_ne!(too_long, device);
        assert!(too_long.contains(&MAX_SECRET_BYTES.to_string()));
    }
}
