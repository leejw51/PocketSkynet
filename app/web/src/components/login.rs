//! Screen 1 — Login, plus the **unlock** state this client adds.
//!
//! Exactly two credentials are accepted, and both are handled entirely in the
//! browser: a **BIP-39 recovery phrase** or a **raw private key in hex**.
//! DESIGN.md §5 also specifies MetaMask and Privy tabs; they are deliberately
//! absent, because both need a JavaScript provider bridge (`window.ethereum`,
//! the Privy SDK) that this bundle does not ship, and a tab that renders but
//! cannot complete a sign-in is worse than no tab at all.
//!
//! Three jobs, in one screen because they are one decision:
//!
//! 1. **Create** a wallet. The mnemonic is generated here and never transmitted.
//!    The submit button stays disabled until the phrase has been copied or
//!    downloaded — you cannot skip past the backup, because there is no account
//!    recovery and nobody to ask.
//! 2. **Import** a wallet, from a phrase or a private key.
//! 3. **Unlock** an existing session. See [`crate::session`]: the JWT survives a
//!    reload but the keys do not, so a returning user re-enters their credential
//!    to restore encryption. It is checked by *deriving* it and comparing
//!    addresses — never by sending anything anywhere. If this device was told to
//!    remember the credential ([`crate::vault`]), the screen does that step
//!    itself and goes straight to the boot sequence.
//!
//! The username field is optional in every one of them. Left blank, the account
//! is named by [`deterministic_username`] from the wallet address — the same
//! name the reference client would pick, so an account has one name no matter
//! which client created it.

use pocketskynet_core::{deterministic_username, wallet, MnemonicLength, Wallet, WalletAddress};
use wasm_bindgen::JsCast;
use web_sys::{HtmlInputElement, HtmlTextAreaElement};
use yew::prelude::*;

use crate::actions;
use crate::i18n::{t, Key, Lang};
use crate::route::Route;
use crate::session::{Auth, LoginLayout, Session, Theme};
use crate::state::{use_store, Action};
use crate::vault::{self, Credential, StoredWallet};

use super::boot::BootSequence;
use super::common::{Addr, BusyButton, Ident, IdentSize, Spinner};
use super::icons;
use super::toast;

/// The two accepted credentials.
///
/// Both end at the same place — a [`Wallet`] — so everything downstream of
/// [`derive_wallet`] is identical. Only the input widget and the validation
/// message differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// A BIP-39 phrase, derived at `m/44'/60'/0'/0/{index}`.
    Mnemonic,
    /// A raw 32-byte secp256k1 scalar, hex, `0x` prefix optional.
    PrivateKey,
}

impl Method {
    fn tab_id(self) -> &'static str {
        match self {
            Self::Mnemonic => "tab-mnemonic",
            Self::PrivateKey => "tab-privatekey",
        }
    }

    fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Mnemonic => t(lang, Key::recovery_phrase),
            Self::PrivateKey => t(lang, Key::private_key),
        }
    }
}

/// Turn whichever credential is active into a wallet.
///
/// The error strings are deliberately specific about *what is wrong with the
/// input* — a wallet that silently derives the wrong address is far worse than
/// one that refuses, because the failure only surfaces later as "nobody can
/// read my messages".
fn derive_wallet(
    lang: Lang,
    method: Method,
    mnemonic: &str,
    private_key: &str,
    index: u32,
) -> Result<Wallet, String> {
    match method {
        Method::Mnemonic => {
            let phrase = mnemonic.trim();
            if phrase.is_empty() {
                return Err(t(lang, Key::enter_recovery_phrase).into());
            }
            if !wallet::validate_mnemonic(phrase) {
                let words = phrase.split_whitespace().count();
                let key = if words == 1 {
                    Key::phrase_word_count_one
                } else {
                    Key::phrase_word_count_many
                };
                return Err(t(lang, key).replace("{n}", &words.to_string()));
            }
            Wallet::from_mnemonic(phrase, index)
                .map_err(|e| t(lang, Key::couldnt_derive_wallet).replace("{error}", &e.to_string()))
        }
        Method::PrivateKey => {
            let raw = private_key.trim();
            if raw.is_empty() {
                return Err(t(lang, Key::enter_private_key).into());
            }
            let hex_part = raw.trim_start_matches("0x").trim_start_matches("0X");
            // Check shape before handing it over, so the message can say which
            // of the two likely mistakes was made.
            if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(t(lang, Key::private_key_hex_only).into());
            }
            if hex_part.len() != 64 {
                let key = if hex_part.len() == 1 {
                    Key::private_key_length_one
                } else {
                    Key::private_key_length_many
                };
                return Err(t(lang, key).replace("{n}", &hex_part.len().to_string()));
            }
            Wallet::from_private_key_hex(raw).map_err(|_| {
                // from_private_key_hex only rejects out-of-range scalars once the
                // shape is known good, so this really is the mathematical case.
                t(lang, Key::private_key_not_scalar).into()
            })
        }
    }
}

#[derive(Properties, PartialEq)]
pub struct LoginProps {
    /// Present when a stored session exists but its keys are gone — the unlock
    /// path. Carries the address to check the entered phrase against.
    #[prop_or_default]
    pub locked_as: Option<(WalletAddress, String)>,
    pub on_navigate: Callback<Route>,
}

#[function_component(Login)]
pub fn login(p: &LoginProps) -> Html {
    let store = use_store();
    let lang = store.language;

    let username = use_state(String::new);
    let method = use_state(|| Method::Mnemonic);
    let mnemonic = use_state(String::new);
    let private_key = use_state(String::new);
    let wallet_index = use_state(|| 0u32);
    let masked = use_state(|| true);
    let busy = use_state(|| false);
    // A verified session waiting behind the boot sequence. Held rather than
    // dispatched so the cold open plays before the app appears — sign-in has
    // already succeeded by the time this is `Some`.
    let booting = use_state(|| Option::<Session>::None);
    let error = use_state(|| Option::<String>::None);
    // Set once a freshly generated phrase has been copied or downloaded.
    let backed_up = use_state(|| false);
    let generated = use_state(|| Option::<String>::None);
    // Whether this device may keep the credential (crate::vault). Read once, so
    // toggling it mid-form does not fight with the stored value.
    let remember = use_state(vault::remember);
    // Set while the vault is signing in on the user's behalf, so the screen can
    // say why the fields filled themselves in.
    let auto_unlocking = use_state(|| false);
    // Appearance and arrangement, both settable *before* signing in. Settings
    // is behind the login screen, so without these the one screen every user
    // sees first is the one screen they cannot adjust.
    let layout = use_state(LoginLayout::load);

    let is_unlock = p.locked_as.is_some();
    let offline = !store.online;

    // --- helpers ---------------------------------------------------------

    let set_error = {
        let error = error.clone();
        Callback::from(move |e: Option<String>| error.set(e))
    };

    let on_username = {
        let username = username.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                username.set(el.value());
            }
        })
    };

    let on_mnemonic = {
        let mnemonic = mnemonic.clone();
        let error = error.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<HtmlTextAreaElement>() {
                mnemonic.set(el.value());
                error.set(None);
            }
        })
    };

    let on_private_key = {
        let private_key = private_key.clone();
        let error = error.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                private_key.set(el.value());
                error.set(None);
            }
        })
    };

    let pick_method = {
        let method = method.clone();
        let error = error.clone();
        move |next: Method| {
            let method = method.clone();
            let error = error.clone();
            Callback::from(move |_: MouseEvent| {
                method.set(next);
                // The old error described the other field; keeping it on screen
                // would read as a complaint about what the user just switched to.
                error.set(None);
            })
        }
    };

    let on_index = {
        let wallet_index = wallet_index.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                // The input is authoritative and typing into it is always
                // allowed; a bad value falls back to 0 rather than being
                // rejected mid-keystroke.
                wallet_index.set(el.value().parse().unwrap_or(0));
            }
        })
    };

    let bump_index = |delta: i64, wallet_index: UseStateHandle<u32>| {
        Callback::from(move |_: MouseEvent| {
            let next = (*wallet_index as i64 + delta).clamp(0, 2_147_483_647);
            wallet_index.set(next as u32);
        })
    };

    // --- generate a new wallet -------------------------------------------

    let on_generate = {
        let mnemonic = mnemonic.clone();
        let username = username.clone();
        let generated = generated.clone();
        let backed_up = backed_up.clone();
        let masked = masked.clone();
        let error = error.clone();
        let wallet_index = wallet_index.clone();
        Callback::from(move |_: MouseEvent| {
            match wallet::generate_mnemonic(MnemonicLength::Words12) {
                Ok(phrase) => {
                    // Stays masked. A recovery phrase is the whole account,
                    // and auto-revealing it puts it on screen — and in any
                    // screen share or shoulder's line of sight — before the
                    // user has decided they are ready to read it. "Copy" and
                    // "Download backup" both work without ever showing it;
                    // revealing is an explicit choice, behind the eye toggle.
                    masked.set(true);
                    backed_up.set(false);
                    // A brand-new wallet has no account yet, and the server
                    // rejects a first login without a name. Filling one in keeps
                    // "create and sign in" from dead-ending on a field the user
                    // was never asked to fill. It is shown rather than merely
                    // implied, because it is the name other people will see.
                    if username.trim().is_empty() {
                        if let Ok(w) = Wallet::from_mnemonic(&phrase, *wallet_index) {
                            username.set(deterministic_username(w.address()));
                        }
                    }
                    generated.set(Some(phrase.clone()));
                    mnemonic.set(phrase);
                    error.set(None);
                    // On a short window the panel appears below the fold, so
                    // the click looks like it did nothing at all.
                    reveal_backup_panel();
                }
                Err(e) => error.set(Some(
                    t(lang, Key::couldnt_generate_wallet).replace("{error}", &e.to_string()),
                )),
            }
        })
    };

    // --- what the entered credential currently points at -------------------

    // The address the fields *would* sign in as, or `None` while the credential
    // is still incomplete. Deriving a phrase costs a PBKDF2 with 2048 rounds, so
    // an invalid one is rejected by the (cheap) checksum first — otherwise every
    // keystroke in the textarea would pay for a derivation that cannot succeed.
    let derived_address = use_memo(
        (
            *method,
            (*mnemonic).clone(),
            (*private_key).clone(),
            *wallet_index,
        ),
        |(method, mnemonic, private_key, index)| {
            if *method == Method::Mnemonic && !wallet::validate_mnemonic(mnemonic.trim()) {
                return None;
            }
            derive_wallet(lang, *method, mnemonic, private_key, *index)
                .ok()
                .map(|w| w.address().clone())
        },
    );

    // The name the account gets if the field is left blank. Shown as the
    // placeholder so it is never a surprise — it is what other members see.
    let suggested_name = (*derived_address).as_ref().map(deterministic_username);

    // Only meaningful while a freshly generated phrase is on screen, and it
    // follows the index stepper: the backup panel must show the address that
    // "Sign in" is actually about to use.
    let new_address = generated
        .is_some()
        .then(|| (*derived_address).clone())
        .flatten();

    let on_copy_phrase = {
        let generated = generated.clone();
        let backed_up = backed_up.clone();
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            if let Some(p) = generated.as_ref() {
                let backed_up = backed_up.clone();
                let store = store.clone();
                super::common::copy_then(p, move |ok| {
                    if ok {
                        backed_up.set(true);
                        toast::success(&store, t(lang, Key::phrase_copied));
                    } else {
                        // Never open the gate on a copy that did not happen: the
                        // user would sign in believing they had saved the one
                        // string that can recover the account.
                        toast::error(
                            &store,
                            "Couldn't copy the phrase",
                            Some(
                                "Your browser blocked clipboard access. Use \"Download backup\", \
                                 or select the phrase and copy it by hand."
                                    .into(),
                            ),
                        );
                    }
                });
            }
        })
    };

    let on_download = {
        let generated = generated.clone();
        let backed_up = backed_up.clone();
        let store = store.clone();
        let new_address = new_address.clone();
        Callback::from(move |_: MouseEvent| {
            let (Some(phrase), Some(address)) = (generated.as_ref(), new_address.as_ref()) else {
                return;
            };
            // Written entirely client-side. The mnemonic never touches the
            // network, not even to be "backed up".
            let json = serde_json::json!({
                "type": "pocketskynet-wallet-backup",
                "version": 1,
                "address": address.as_str(),
                "mnemonic": phrase,
            });
            download_json(
                &format!("pocketskynet-{}.json", address.abbreviated()),
                &json.to_string(),
            );
            backed_up.set(true);
            toast::success(&store, t(lang, Key::backup_downloaded));
        })
    };

    // --- submit ----------------------------------------------------------

    let on_submit = {
        let store = store.clone();
        let username = username.clone();
        let method = method.clone();
        let mnemonic = mnemonic.clone();
        let private_key = private_key.clone();
        let wallet_index = wallet_index.clone();
        let busy = busy.clone();
        let set_error = set_error.clone();
        let booting = booting.clone();
        let remember = remember.clone();
        let locked_as = p.locked_as.clone();

        Callback::from(move |_: ()| {
            if *busy {
                return;
            }
            let wallet = match derive_wallet(lang, *method, &mnemonic, &private_key, *wallet_index)
            {
                Ok(w) => w,
                Err(e) => {
                    set_error.emit(Some(e));
                    return;
                }
            };

            // Unlock path: the credential must derive the address we already
            // hold. Either method is fine — what matters is the address, not
            // how it was reached.
            if let Some((expected, _)) = &locked_as {
                if wallet.address() != expected {
                    set_error.emit(Some(
                        "That belongs to a different wallet. Use \"Sign in as someone else\" if \
                         you meant to switch accounts."
                            .into(),
                    ));
                    return;
                }
            }

            busy.set(true);
            set_error.emit(None);
            let store = store.clone();
            let busy = busy.clone();
            let set_error = set_error.clone();
            let booting = booting.clone();
            // Blank is not an error — it means "name me". The server would
            // reject an empty username on a first login, and inventing a name
            // is a decision nobody should have to make before they have seen
            // their own wallet. Derived from the address, so it is the same
            // name any other client would have chosen for this account.
            let username = match username.trim() {
                "" => deterministic_username(wallet.address()),
                chosen => chosen.to_owned(),
            };
            let credential = credential_of(*method, &mnemonic, &private_key, *wallet_index);
            let address = wallet.address().clone();
            let remember = *remember;
            let client = store.client.clone();

            wasm_bindgen_futures::spawn_local(async move {
                match actions::sign_in(client, wallet, &username).await {
                    Ok(session) => {
                        // Persist immediately: the session is real from here,
                        // and a reload mid-cutscene should land signed in
                        // rather than throw the credential away.
                        session.persist();
                        // The username the *server* settled on, which for a
                        // returning user is the one it already had rather than
                        // the one just sent.
                        remember_wallet(remember, &session.user.username, address, credential);
                        // `busy` deliberately stays set: the form is inert
                        // behind the cutscene until this component unmounts.
                        booting.set(Some(session));
                    }
                    Err(e) => {
                        // Both, because the toast can be missed and the inline
                        // block can be scrolled out of view (DESIGN.md §5).
                        set_error.emit(Some(e.clone()));
                        toast::error(&store, t(lang, Key::couldnt_sign_in), Some(e));
                        busy.set(false);
                    }
                }
            });
        })
    };

    // --- the vault: sign in again without asking ---------------------------

    // Runs once, on mount. If this device was told to remember the credential
    // (crate::vault), the unlock step is done here rather than being asked for
    // again — which is the entire point of storing it.
    //
    // It does not go through `on_submit`: that callback closed over the field
    // values as they were at *this* render, so filling the fields in and then
    // emitting it would sign in with the empty strings it captured. The fields
    // are still populated, because the user should be able to see what the
    // screen is signing in with — and to correct it if the attempt fails.
    {
        let store = store.clone();
        let method = method.clone();
        let mnemonic = mnemonic.clone();
        let private_key = private_key.clone();
        let wallet_index = wallet_index.clone();
        let username = username.clone();
        let busy = busy.clone();
        let booting = booting.clone();
        let set_error = set_error.clone();
        let auto_unlocking = auto_unlocking.clone();
        let locked_as = p.locked_as.clone();
        let already_unlocked = store.auth.can_decrypt();

        use_effect_with((), move |()| {
            let stored = match &locked_as {
                // Unlocking: only a vault for *this* account will do. One for
                // another wallet must never sign in over the session that is
                // already on the device.
                Some((address, _)) => StoredWallet::load_for(address),
                None => StoredWallet::load(),
            };
            let Some(stored) = stored else { return };

            match &stored.credential {
                Credential::Mnemonic { phrase, index } => {
                    method.set(Method::Mnemonic);
                    mnemonic.set(phrase.clone());
                    wallet_index.set(*index);
                }
                Credential::PrivateKey { hex } => {
                    method.set(Method::PrivateKey);
                    private_key.set(hex.clone());
                }
            }
            username.set(stored.username.clone());

            // Signed out rather than locked: the fields are filled in, but the
            // sign-in itself stays a deliberate click. Nothing on screen said
            // this device was still holding a session, so acting on it would be
            // a surprise.
            //
            // `already_unlocked` covers visiting `/login` with a working
            // session — which renders this screen, but has nothing to unlock.
            // Signing in again would spend a challenge and a login against a
            // 5-per-minute limiter to reach the state it is already in.
            if locked_as.is_none() || already_unlocked {
                return;
            }
            let Some(wallet) = stored.wallet() else {
                return;
            };

            auto_unlocking.set(true);
            busy.set(true);
            let client = store.client.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match actions::sign_in(client, wallet, &stored.username).await {
                    Ok(session) => {
                        session.persist();
                        booting.set(Some(session));
                    }
                    Err(e) => {
                        // The credential is *not* discarded here. A failure at
                        // this point is nearly always the network, and throwing
                        // away the phrase over a flaky connection would be
                        // unrecoverable. The form is left ready to retry.
                        set_error.emit(Some(e));
                        auto_unlocking.set(false);
                        busy.set(false);
                    }
                }
            });
        });
    }

    let submit_click = {
        let on_submit = on_submit.clone();
        Callback::from(move |_: MouseEvent| on_submit.emit(()))
    };

    let on_keydown = {
        let on_submit = on_submit.clone();
        Callback::from(move |e: KeyboardEvent| {
            // Enter submits from the single-line fields; the textarea needs
            // Ctrl/Cmd+Enter so a phrase can still contain a newline.
            let from_textarea = e
                .target()
                .and_then(|t| t.dyn_into::<HtmlTextAreaElement>().ok())
                .is_some();
            if e.key() == "Enter" && (!from_textarea || e.ctrl_key() || e.meta_key()) {
                e.prevent_default();
                on_submit.emit(());
            }
        })
    };

    let sign_out = {
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            crate::session::PersistedSession::clear();
            // Switching accounts forgets the remembered one. Leaving it behind
            // would auto-unlock straight back into the account the user just
            // said they were leaving. `clear`, not `forget`: the *preference*
            // survives, because switching accounts is usually a prelude to
            // remembering a different one.
            vault::clear();
            store.dispatch(Action::SetAuth(Auth::SignedOut));
        })
    };

    let set_theme = {
        let store = store.clone();
        move |t: Theme| {
            let store = store.clone();
            Callback::from(move |_: MouseEvent| store.dispatch(Action::SetTheme(t)))
        }
    };

    let set_layout = {
        let layout = layout.clone();
        move |next: LoginLayout| {
            let layout = layout.clone();
            Callback::from(move |_: MouseEvent| {
                // Clicking the layout already in effect returns to `Auto`, so
                // the two buttons are also the way *out* of a pinned layout —
                // otherwise the first click is irreversible and the screen
                // stops responding to the window for good.
                let next = if *layout == next {
                    LoginLayout::Auto
                } else {
                    next
                };
                next.save();
                layout.set(next);
            })
        }
    };

    // A freshly generated phrase blocks submission until it is backed up. It
    // only applies to the phrase tab — a pasted private key was never ours to
    // lose.
    let must_back_up = *method == Method::Mnemonic && generated.is_some() && !*backed_up;
    // The backup panel carries its own submit ("Save the phrase to continue"
    // → "Sign in") because saving and signing in are one step in that flow.
    // While it is on screen the sticky bar must not render a second one — two
    // identical primary buttons on the same screen is a coin toss, not a
    // choice, and the one in the panel is the one with the context.
    let backup_panel_visible = generated.is_some() && new_address.is_some();
    let credential_empty = match *method {
        Method::Mnemonic => mnemonic.trim().is_empty(),
        Method::PrivateKey => private_key.trim().is_empty(),
    };
    let submit_disabled = *busy || credential_empty || must_back_up || offline;

    // The cutscene is over: hand the already-verified session to the store and
    // let the app take the screen. Everything here used to run the instant

    // Sign in with the browser wallet. Three prompts, and the wallet key never
    // reaches this process — see `actions::sign_in_with_wallet`.
    let on_wallet = {
        let store = store.clone();
        let busy = busy.clone();
        let set_error = set_error.clone();
        let booting = booting.clone();
        let username = (*username).clone();
        let chain = store.chain.chain_id_num();
        Callback::from(move |_: MouseEvent| {
            let store = store.clone();
            let busy = busy.clone();
            let set_error = set_error.clone();
            let booting = booting.clone();
            // Blank means "name me", same as the phrase path. The name is
            // derived from the address the wallet reports, so it matches what
            // any other client would pick for the same account.
            let typed = username.trim().to_owned();
            let client = store.client.clone();
            busy.set(true);
            set_error.emit(None);

            wasm_bindgen_futures::spawn_local(async move {
                let lang = store.language;
                let Some(provider) = crate::eip1193::Provider::injected() else {
                    set_error.emit(Some(t(lang, Key::wallet_not_found).to_owned()));
                    busy.set(false);
                    return;
                };
                match actions::sign_in_with_wallet(client, provider, &typed, chain).await {
                    Ok(session) => {
                        session.persist();
                        // Nothing is remembered for a wallet session: there is no
                        // credential to keep. The wallet is the credential, and it
                        // re-prompts next time — which is the point of using it.
                        booting.set(Some(session));
                    }
                    Err(e) => {
                        set_error.emit(Some(e.clone()));
                        toast::error(&store, t(lang, Key::couldnt_sign_in), Some(e));
                        busy.set(false);
                    }
                }
            });
        })
    };

    // Privy: load the bundle on demand, open its modal, then take the EIP-1193
    // provider it hands back and run the *same* flow MetaMask runs.
    let on_privy = {
        let store = store.clone();
        let busy = busy.clone();
        let set_error = set_error.clone();
        let booting = booting.clone();
        let username = (*username).clone();
        let chain = store.chain.clone();
        let app_id = store.chain.privy_app_id.clone();
        Callback::from(move |_: MouseEvent| {
            let store = store.clone();
            let busy = busy.clone();
            let set_error = set_error.clone();
            let booting = booting.clone();
            let typed = username.trim().to_owned();
            let client = store.client.clone();
            let app_id = app_id.clone();
            let want = chain.chain_id_num();
            let chain_cfg = want.map(|id| crate::privy::Chain {
                id,
                name: chain.chain_name.clone(),
                rpc: chain.chain_rpc.clone(),
                explorer: chain.chain_explorer.clone(),
            });
            busy.set(true);
            set_error.emit(None);

            wasm_bindgen_futures::spawn_local(async move {
                let lang = store.language;
                let provider = match crate::privy::connect(&app_id, chain_cfg).await {
                    Ok(p) => p,
                    Err(e) => {
                        let msg = t(lang, e.key()).to_owned();
                        set_error.emit(Some(msg.clone()));
                        toast::error(&store, t(lang, Key::couldnt_sign_in), Some(msg));
                        busy.set(false);
                        return;
                    }
                };
                match actions::sign_in_with_wallet(client, provider, &typed, want).await {
                    Ok(session) => {
                        session.persist();
                        booting.set(Some(session));
                    }
                    Err(e) => {
                        set_error.emit(Some(e.clone()));
                        toast::error(&store, t(lang, Key::couldnt_sign_in), Some(e));
                        busy.set(false);
                    }
                }
            });
        })
    };

    // `sign_in` returned; the only change is *when*.
    let on_boot_done = {
        let store = store.clone();
        let booting = booting.clone();
        let on_navigate = p.on_navigate.clone();
        Callback::from(move |_: ()| {
            let Some(session) = (*booting).clone() else {
                return;
            };
            let name = session.user.display_name();
            store.dispatch(Action::SetAuth(Auth::Unlocked(session)));
            toast::success(&store, t(lang, Key::signed_in_as).replace("{name}", &name));
            on_navigate.emit(Route::Rooms);
        })
    };

    if let Some(session) = &*booting {
        return html! {
            <BootSequence
                username={session.user.display_name()}
                on_done={on_boot_done}
            />
        };
    }

    html! {
        // Two panels: the form, and the artwork. `fn-art fn-art--login` only
        // supplies the hero image's light/dark pair — app.css decides whether
        // it is a full-height panel beside the form (wide viewports) or a
        // banner above it (everything else), unless `data-layout` pins it.
        <main class="fn-login" data-layout={layout.attribute()}>
            <div class="fn-login__panel">
            <div class="fn-login__card" onkeydown={on_keydown}>
                // Appearance and arrangement, above the lockup. Both are
                // preferences about *this screen*, and Settings — where they
                // otherwise live — is on the far side of a sign-in.
                <div class="fn-login__prefs">
                    // First control on the screen, and deliberately so: this
                    // is the one page a reader who cannot follow English has
                    // no way past — Settings is on the other side of a sign-in
                    // they cannot read. Endonyms, so finding your language
                    // never requires reading another one.
                    <div class="fn-seg fn-seg--wrap" role="group" aria-label={t(lang, Key::language)}>
                        { for Lang::ALL.into_iter().map(|l| {
                            let store = store.clone();
                            html! {
                                <button
                                    type="button"
                                    class="fn-seg__btn"
                                    lang={l.tag()}
                                    aria-pressed={(lang == l).to_string()}
                                    onclick={Callback::from(move |_: MouseEvent| {
                                        store.dispatch(Action::SetLanguage(l));
                                    })}
                                ><span>{ l.endonym() }</span></button>
                            }
                        }) }
                    </div>
                    <div class="fn-seg" role="group" aria-label={t(lang, Key::appearance)}>
                        { for [(Theme::Light, t(lang, Key::theme_light), icons::moon_sun(15)),
                               (Theme::Dark, t(lang, Key::theme_dark), icons::moon(15))]
                            .into_iter().map(|(t, label, glyph)| html! {
                                <button
                                    type="button"
                                    class="fn-seg__btn"
                                    aria-pressed={(store.theme == t).to_string()}
                                    onclick={set_theme(t)}
                                >{ glyph }<span>{ label }</span></button>
                            }) }
                    </div>
                    <div class="fn-seg" role="group" aria-label={t(lang, Key::layout)}>
                        { for [(LoginLayout::Vertical, t(lang, Key::login_vertical), icons::rows(15)),
                               (LoginLayout::Horizontal, t(lang, Key::login_horizontal), icons::columns(15))]
                            .into_iter().map(|(l, label, glyph)| html! {
                                <button
                                    type="button"
                                    class="fn-seg__btn"
                                    aria-pressed={(*layout == l).to_string()}
                                    // Says what a second click does, since
                                    // returning to Auto is not something a
                                    // pressed toggle usually offers.
                                    title={if *layout == l {
                                        "Click again to follow the window size"
                                    } else if l == LoginLayout::Vertical {
                                        t(lang, Key::artwork_above)
                                    } else {
                                        t(lang, Key::form_beside)
                                    }}
                                    onclick={set_layout(l)}
                                >{ glyph }<span>{ label }</span></button>
                            }) }
                    </div>
                </div>

                <div class="fn-login__brand">
                    // The lockup is the wordmark and one lit rule above it.
                    // The app icon used to sit here and read as a 44px grey
                    // square on a black panel — an asset shown at the size
                    // where it stops being legible adds nothing.
                    <span class="fn-login__mark" aria-hidden="true" />
                    <h1 class="fn-login__wordmark">
                        { "PocketSkynet" }
                        <span class="fn-login__version">
                            { concat!("v", env!("CARGO_PKG_VERSION")) }
                        </span>
                    </h1>
                    <p class="fn-login__tagline">
                        if is_unlock {
                            { t(lang, Key::unlock_tagline).replace("{method}", &method.label(lang).to_lowercase()) }
                        } else {
                            { t(lang, Key::sign_in_tagline) }
                        }
                    </p>
                </div>

                if offline {
                    <div class="fn-banner fn-banner--offline" role="status">
                        { t(lang, Key::offline_can_still_create) }
                    </div>
                }

                if *auto_unlocking {
                    <div class="fn-banner" role="status">
                        <Spinner />
                        { " " }{ t(lang, Key::unlocking_with_saved_phrase) }
                    </div>
                }

                if let Some((address, name)) = &p.locked_as {
                    <div class="fn-picklist__row">
                        <Ident seed={address.to_string()} size={IdentSize::Lg} is_self=true />
                        <div class="fn-grow">
                            <strong>{ name }</strong>
                            <div><Addr address={address.clone()} /></div>
                        </div>
                        <button type="button" class="topcoat-button" onclick={sign_out}>
                            { t(lang, Key::sign_in_as_someone_else) }
                        </button>
                    </div>
                } else {
                    <div class="fn-login__hero">
                        <button
                            type="button"
                            class="fn-hero-btn topcoat-button--large--cta"
                            onclick={on_generate.clone()}
                        >
                            { icons::bolt(20) }
                            <span class="fn-hero-btn__label">
                                <b>{ t(lang, Key::create_wallet_and_sign_in) }</b>
                                <small>{ t(lang, Key::new_phrase_tagline) }</small>
                            </span>
                        </button>
                        // Only when a provider is actually injected: a button
                        // that can only ever say "install MetaMask" is worse
                        // than no button.
                        if crate::eip1193::available() {
                            <button
                                type="button"
                                class="fn-hero-btn fn-hero-btn--wallet topcoat-button--large"
                                disabled={*busy}
                                onclick={on_wallet.clone()}
                            >
                                { icons::wallet(20) }
                                <span class="fn-hero-btn__label">
                                    <b>{ t(lang, Key::wallet_signin) }</b>
                                    <small>{
                                        if *busy { t(lang, Key::wallet_connecting) }
                                        else { t(lang, Key::wallet_signin_hint) }
                                    }</small>
                                </span>
                            </button>
                        }
                        // The iOS/Android case. There is no extension to inject a
                        // provider on a phone — MetaMask is an app — so the only
                        // thing that can work is reopening this page inside
                        // MetaMask's own browser, which does inject one. Shown
                        // only when there is no provider *and* this looks like a
                        // phone, so MetaMask's in-app browser (which has one)
                        // gets the real sign-in button above instead.
                        if !crate::eip1193::available() && crate::eip1193::is_mobile() {
                            if let Some(link) = crate::eip1193::metamask_deeplink() {
                                <a
                                    class="fn-hero-btn fn-hero-btn--wallet topcoat-button--large"
                                    href={link}
                                    rel="noopener"
                                >
                                    { icons::wallet(20) }
                                    <span class="fn-hero-btn__label">
                                        <b>{ t(lang, Key::wallet_open_in_metamask) }</b>
                                        <small>{ t(lang, Key::wallet_ios_hint) }</small>
                                    </span>
                                </a>
                            }
                        }

                        // The certificate. Shown whenever this server generated
                        // its own, because the person who needs it most cannot
                        // be detected: they are on a phone, in MetaMask's
                        // in-app browser, looking at a warning with no way past
                        // it — so they never reach this page to be detected at
                        // all. The link has to be here, in the browser that
                        // *did* get through, so it can be used to fix the one
                        // that did not.
                        if store.chain.ca_cert_available {
                            <details class="fn-trust">
                                <summary>{ t(lang, Key::trust_server) }</summary>
                                <p class="fn-trust__why">{ t(lang, Key::trust_server_why) }</p>
                                <a class="fn-trust__get" href="/ca.crt" download="pocketskynet-ca.crt">
                                    { icons::download(16) }
                                    { t(lang, Key::trust_server) }
                                </a>
                                <p class="fn-trust__steps">{ t(lang, Key::trust_server_ios) }</p>
                            </details>
                        }

                        // Offered only when the server supplied an app id, which
                        // is exactly how the reference client gates it.
                        if !store.chain.privy_app_id.trim().is_empty() {
                            <button
                                type="button"
                                class="fn-hero-btn fn-hero-btn--wallet topcoat-button--large"
                                disabled={*busy}
                                onclick={on_privy.clone()}
                            >
                                { icons::envelope(20) }
                                <span class="fn-hero-btn__label">
                                    <b>{ t(lang, Key::privy_signin) }</b>
                                    <small>{
                                        if *busy { t(lang, Key::privy_loading) }
                                        else { t(lang, Key::privy_signin_hint) }
                                    }</small>
                                </span>
                            </button>
                        }
                    </div>
                }

                // Outside the branch above on purpose. Unlocking needs the same
                // choice of credential as signing in: a session created with a
                // private key cannot be unlocked with a recovery phrase, and
                // offering only the phrase field stranded those users with no
                // way back in except signing out entirely.
                if !is_unlock {
                    <div class="fn-rule">{ t(lang, Key::or_sign_in_with) }</div>
                }
                <div class="fn-tabs" role="tablist" aria-label={t(lang, Key::sign_in_method)}>
                    { for [Method::Mnemonic, Method::PrivateKey].into_iter().map(|m| {
                        let selected = *method == m;
                        html! {
                            <button
                                type="button"
                                class="fn-tab"
                                role="tab"
                                id={m.tab_id()}
                                aria-selected={selected.to_string()}
                                tabindex={if selected { "0" } else { "-1" }}
                                onclick={pick_method(m)}
                            >
                                { m.label(lang) }
                            </button>
                        }
                    }) }
                </div>

                <div class="fn-tabpanel" role="tabpanel" aria-labelledby={method.tab_id()}>
                    if !is_unlock {
                        <div class="fn-field">
                            <div class="fn-row">
                                <label class="fn-field__label fn-grow" for="login-username">
                                    { t(lang, Key::username) }
                                </label>
                            </div>
                            <input
                                id="login-username"
                                class="topcoat-text-input"
                                type="text"
                                autocomplete="username"
                                // The name that will be used if this is left
                                // empty, shown rather than merely promised —
                                // it is what everyone else in a room sees.
                                placeholder={suggested_name.clone()}
                                value={(*username).clone()}
                                oninput={on_username}
                            />
                            <p class="fn-field__help">
                                if let Some(name) = &suggested_name {
                                    { t(lang, Key::username_suggested_hint).replace("{name}", name) }
                                } else {
                                    { t(lang, Key::username_blank_hint) }
                                }
                            </p>
                        </div>
                    }

                    if *method == Method::PrivateKey {
                        <div class="fn-field">
                            <div class="fn-row">
                                <label class="fn-field__label fn-grow" for="login-privatekey">
                                    { t(lang, Key::private_key) }
                                </label>
                                <span class="fn-mnemonic__tools">
                                    <button
                                        type="button"
                                        class="topcoat-icon-button--quiet"
                                        aria-label={if *masked { t(lang, Key::show_private_key) } else { t(lang, Key::hide_private_key) }}
                                        aria-pressed={(!*masked).to_string()}
                                        onclick={{
                                            let masked = masked.clone();
                                            Callback::from(move |_: MouseEvent| masked.set(!*masked))
                                        }}
                                    >
                                        { if *masked { icons::eye(16) } else { icons::eye_off(16) } }
                                    </button>
                                    <button
                                        type="button"
                                        class="topcoat-icon-button--quiet"
                                        aria-label={t(lang, Key::clear_private_key)}
                                        onclick={{
                                            let private_key = private_key.clone();
                                            Callback::from(move |_: MouseEvent| {
                                                private_key.set(String::new());
                                            })
                                        }}
                                    >
                                        { icons::close(16) }
                                    </button>
                                </span>
                            </div>
                            <input
                                id="login-privatekey"
                                class="topcoat-text-input fn-mono"
                                // `password` so browsers and screen-sharing
                                // treat it as a secret; the eye toggle is the
                                // deliberate way to reveal it.
                                type={if *masked { "password" } else { "text" }}
                                spellcheck="false"
                                autocapitalize="none"
                                autocomplete="off"
                                placeholder="0x…"
                                aria-invalid={error.is_some().then_some("true")}
                                aria-describedby={error.is_some().then_some("login-error")}
                                value={(*private_key).clone()}
                                oninput={on_private_key}
                            />
                            <p class="fn-field__help">
                                { t(lang, Key::private_key_hint) }
                            </p>
                        </div>
                    }

                    if *method == Method::Mnemonic {
                    <div class="fn-field">
                        <div class="fn-row">
                            <label class="fn-field__label fn-grow" for="login-mnemonic">
                                { t(lang, Key::recovery_phrase) }
                            </label>
                            <span class="fn-mnemonic__tools">
                                // Labelled, not icon-only. The phrase is hidden
                                // by default now, so the way to reveal it has to
                                // be obvious rather than a glyph the user has to
                                // recognise.
                                <button
                                    type="button"
                                    class="topcoat-button fn-reveal"
                                    aria-label={if *masked { t(lang, Key::show_phrase) } else { t(lang, Key::hide_phrase) }}
                                    aria-pressed={(!*masked).to_string()}
                                    onclick={{
                                        let masked = masked.clone();
                                        Callback::from(move |_: MouseEvent| masked.set(!*masked))
                                    }}
                                >
                                    { if *masked { icons::eye(16) } else { icons::eye_off(16) } }
                                    <span>{ if *masked { t(lang, Key::show) } else { t(lang, Key::hide) } }</span>
                                </button>
                                <button
                                    type="button"
                                    class="topcoat-icon-button--quiet"
                                    aria-label={t(lang, Key::clear_phrase)}
                                    onclick={{
                                        let mnemonic = mnemonic.clone();
                                        let generated = generated.clone();
                                        Callback::from(move |_: MouseEvent| {
                                            mnemonic.set(String::new());
                                            generated.set(None);
                                        })
                                    }}
                                >
                                    { icons::close(16) }
                                </button>
                            </span>
                        </div>
                        <div class="fn-mnemonic" data-masked={masked.to_string()}>
                            <textarea
                                id="login-mnemonic"
                                class="topcoat-textarea"
                                rows="3"
                                spellcheck="false"
                                autocapitalize="none"
                                autocomplete="off"
                                aria-invalid={error.is_some().then_some("true")}
                                aria-describedby={error.is_some().then_some("login-error")}
                                value={(*mnemonic).clone()}
                                oninput={on_mnemonic}
                            />
                        </div>
                        if !is_unlock {
                            <button type="button" class="topcoat-button" onclick={on_generate}>
                                { t(lang, Key::generate_new_phrase) }
                            </button>
                        }
                    </div>
                    }

                    if let (Some(_), Some(address)) = ((*generated).clone(), new_address.clone()) {
                        <div class="fn-warnpanel" role="note">
                            <p class="fn-warnpanel__title">{ t(lang, Key::save_phrase_now) }</p>
                            <ul>
                                <li>{ t(lang, Key::phrase_only_way_back) }</li>
                                <li>{ t(lang, Key::phrase_nobody_recovers) }</li>
                                <li>{ t(lang, Key::phrase_anyone_reads) }</li>
                            </ul>
                            <p><Addr address={address} full=true /></p>
                            <div class="fn-row">
                                <button type="button" class="topcoat-button" onclick={on_copy_phrase}>
                                    { icons::copy(16) }{ " " }{ t(lang, Key::copy_phrase) }
                                </button>
                                <button type="button" class="topcoat-button" onclick={on_download}>
                                    { icons::download(16) }{ " " }{ t(lang, Key::download_backup) }
                                </button>
                            </div>
                            // The hero button says "create a wallet and sign in".
                            // Without this the flow stopped here: a phrase
                            // appeared, the real submit sat further down the page
                            // disabled, and nothing said the click had worked.
                            // Saving and signing in are one step now.
                            <BusyButton
                                label={if must_back_up {
                                    t(lang, Key::save_phrase_to_continue).to_string()
                                } else {
                                    t(lang, Key::sign_in).to_string()
                                }}
                                class="topcoat-button--large--cta"
                                busy={*busy}
                                disabled={submit_disabled}
                                onclick={submit_click.clone()}
                            />
                            if must_back_up {
                                <p class="fn-field__help">
                                    { t(lang, Key::back_up_first_hint) }
                                </p>
                            }
                        </div>
                    }

                    // Derivation index is a property of a BIP-32 tree. A raw
                    // private key has no tree, so offering the stepper there
                    // would imply a choice that does not exist.
                    if !is_unlock && *method == Method::Mnemonic {
                        <div class="fn-field">
                            <label class="fn-field__label" for="login-index">{ t(lang, Key::wallet_index) }</label>
                            <div class="fn-stepper">
                                <button
                                    type="button"
                                    class="topcoat-button"
                                    aria-label={t(lang, Key::prev_wallet_index)}
                                    onclick={bump_index(-1, wallet_index.clone())}
                                >{ "−" }</button>
                                <input
                                    id="login-index"
                                    class="topcoat-text-input"
                                    type="number"
                                    min="0"
                                    inputmode="numeric"
                                    value={wallet_index.to_string()}
                                    oninput={on_index}
                                />
                                <button
                                    type="button"
                                    class="topcoat-button"
                                    aria-label={t(lang, Key::next_wallet_index)}
                                    onclick={bump_index(1, wallet_index.clone())}
                                >{ "+" }</button>
                            </div>
                        </div>
                    }

                    // Next to the credential it applies to, not buried in
                    // Settings, because it decides whether the thing typed into
                    // the field above outlives the tab (crate::vault).
                    <div class="fn-toggle-row" data-on={remember.to_string()}>
                        <label class="fn-row" for="login-remember">
                            <input
                                id="login-remember"
                                type="checkbox"
                                class="topcoat-checkbox__input"
                                checked={*remember}
                                onchange={{
                                    let remember = remember.clone();
                                    Callback::from(move |_: Event| {
                                        let next = !*remember;
                                        // Applied immediately, not on submit:
                                        // turning it off must erase a
                                        // credential stored earlier even if the
                                        // user never signs in on this visit.
                                        vault::set_remember(next);
                                        remember.set(next);
                                    })
                                }}
                            />
                            <span class="fn-grow">
                                <strong>{ t(lang, Key::stay_signed_in) }</strong>
                                <p class="fn-field__help">
                                    if *remember {
                                        { t(lang, Key::stay_signed_in_hint) }
                                    } else {
                                        { t(lang, Key::nothing_is_kept_hint) }
                                    }
                                </p>
                            </span>
                        </label>
                    </div>

                    if let Some(e) = &*error {
                        <p class="fn-login__error" id="login-error" role="alert">{ e }</p>
                    }

                    // Grouped, not loose siblings, so that on a viewport too
                    // short to show the whole form this bar can stick to the
                    // bottom of the card while the fields scroll behind it.
                    // A sign-in screen whose submit button is off-screen when
                    // it loads reads as broken even though the page scrolls.
                    <div class="fn-login__actions">
                        if !backup_panel_visible {
                            <BusyButton
                                label={if is_unlock { t(lang, Key::unlock) } else { t(lang, Key::sign_in) }.to_string()}
                                class="topcoat-button--large--cta"
                                busy={*busy}
                                disabled={submit_disabled}
                                onclick={submit_click}
                            />
                        }
                        if *busy {
                            <p class="fn-muted"><Spinner />{ " " }{ t(lang, Key::signing_the_challenge) }</p>
                        }
                        <p class="fn-field__help">{ t(lang, Key::wallet_address_is_account) }</p>
                    </div>
                </div>
            </div>
            </div>

            // The backdrop: the hero illustration, full-bleed and fixed, with
            // the scrim that buys the form its contrast painted over it. Purely
            // decorative, so it carries no text and no accessible name.
            <div class="fn-login__aside fn-art fn-art--login" aria-hidden="true">
                // The GPU layer, over the CSS artwork and under the form. It
                // renders nothing at all when WebGL2 is missing, the shader
                // will not compile, or reduced motion is preferred — the CSS
                // backdrop underneath is the fallback, and it is complete.
                <super::backdrop::GlBackdrop layout={layout.attribute().unwrap_or("auto").to_string()} />
            </div>
        </main>
    }
}

/// Trigger a client-side download. No server round trip — the file being saved
/// is the one thing that must never leave the device by any other route.
#[cfg(target_arch = "wasm32")]
fn download_json(filename: &str, contents: &str) {
    use wasm_bindgen::JsValue;

    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let array = js_sys::Array::new();
    array.push(&JsValue::from_str(contents));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/json");
    let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(&array, &opts) else {
        return;
    };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else {
        return;
    };
    if let Ok(anchor) = document.create_element("a") {
        if let Ok(anchor) = anchor.dyn_into::<web_sys::HtmlAnchorElement>() {
            anchor.set_href(&url);
            anchor.set_download(filename);
            anchor.click();
        }
    }
    // Revoking immediately is safe: the click has already queued the download.
    let _ = web_sys::Url::revoke_object_url(&url);
}

#[cfg(not(target_arch = "wasm32"))]
fn download_json(_filename: &str, _contents: &str) {}

/// The credential exactly as the form holds it, for [`crate::vault`].
///
/// Trimmed, because what is stored has to re-derive on the next visit and a
/// pasted phrase routinely arrives with a trailing newline. The index rides
/// along with the phrase — the same words at index 1 are a different account,
/// so a phrase stored without its index would silently sign the user in as
/// somebody else.
fn credential_of(method: Method, mnemonic: &str, private_key: &str, index: u32) -> Credential {
    match method {
        Method::Mnemonic => Credential::Mnemonic {
            phrase: mnemonic.trim().to_owned(),
            index,
        },
        Method::PrivateKey => Credential::PrivateKey {
            hex: private_key.trim().to_owned(),
        },
    }
}

/// Persist — or deliberately un-persist — the credential after a sign-in.
///
/// The preference is written on **every** sign-in, not only when remembering is
/// on: `set_remember(false)` also wipes the vault, which is what makes signing
/// in with the switch off clear a credential stored by an earlier session.
fn remember_wallet(
    remember: bool,
    username: &str,
    wallet_address: WalletAddress,
    credential: Credential,
) {
    vault::set_remember(remember);
    if remember {
        StoredWallet {
            username: username.to_owned(),
            wallet_address,
            credential,
        }
        .save();
    }
}

/// Scroll the freshly revealed backup panel into view.
///
/// On a short window the panel renders below the fold, so clicking "Create a
/// wallet and sign in" looked like it had done nothing whatsoever.
#[cfg(target_arch = "wasm32")]
fn reveal_backup_panel() {
    use wasm_bindgen::JsCast;

    // Deferred: the panel does not exist until Yew has rendered this state.
    let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        if let Some(el) = doc.query_selector(".fn-warnpanel").ok().flatten() {
            if let Ok(el) = el.dyn_into::<web_sys::HtmlElement>() {
                // Centred, not top-aligned: the panel is the tallest thing on
                // the page and aligning its top pushes the action below the
                // fold again, which is the problem this is here to solve.
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_block(web_sys::ScrollLogicalPosition::Center);
                opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                el.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        }
    });
    if let Some(win) = web_sys::window() {
        let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 0);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn reveal_backup_panel() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A known-good BIP-39 English phrase (the all-`abandon` test vector) and
    /// the address it derives at index 0 — the same pair MetaMask produces.
    const PHRASE: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const PHRASE_ADDR: &str = "0x9858effd232b4033e47d90003d41ec34ecaeda94";

    /// secp256k1 scalar 1 — a valid key with a well-known address.
    const KEY_ONE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const KEY_ONE_ADDR: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";

    fn derive(method: Method, mnemonic: &str, key: &str, index: u32) -> Result<String, String> {
        derive_wallet(Lang::En, method, mnemonic, key, index).map(|w| w.address().to_string())
    }

    #[test]
    fn a_new_wallet_is_given_a_name_so_first_login_cannot_dead_end() {
        // The server refuses a first login with no username. "Create a wallet
        // and sign in" therefore has to supply one, or the button cannot keep
        // its promise. It comes from the *address*, so the same wallet is the
        // same person whichever client — or credential — reached it.
        let wallet = Wallet::from_mnemonic(PHRASE, 0).unwrap();
        let name = deterministic_username(wallet.address());
        assert_eq!(name, "AmberEnchanter2784");

        // The private key for the same account produces the same name, which is
        // the property that makes it safe to fill in silently.
        let by_key = Wallet::from_private_key_hex(&wallet.private_key_hex()).unwrap();
        assert_eq!(deterministic_username(by_key.address()), name);

        // A different index is a different account and gets a different name.
        let other = Wallet::from_mnemonic(PHRASE, 1).unwrap();
        assert_ne!(deterministic_username(other.address()), name);
    }

    #[test]
    fn the_credential_stored_for_next_time_is_the_one_that_was_used() {
        // Storing the wrong tab's field would remember an account the user
        // never signed in as — and, because both derive successfully, it would
        // do it silently.
        let by_phrase = credential_of(Method::Mnemonic, PHRASE, KEY_ONE, 0);
        assert_eq!(
            by_phrase,
            Credential::Mnemonic {
                phrase: PHRASE.into(),
                index: 0
            }
        );

        let by_key = credential_of(Method::PrivateKey, PHRASE, KEY_ONE, 7);
        assert_eq!(
            by_key,
            Credential::PrivateKey {
                hex: KEY_ONE.into()
            },
            "a raw key has no derivation tree, so the index must not ride along"
        );

        // Whitespace is trimmed: a pasted phrase routinely arrives with a
        // trailing newline, and what is stored has to re-derive next time.
        assert_eq!(
            credential_of(Method::Mnemonic, &format!("  {PHRASE}\n"), "", 3),
            Credential::Mnemonic {
                phrase: PHRASE.into(),
                index: 3
            }
        );
    }

    #[test]
    fn a_mnemonic_derives_the_same_address_metamask_would() {
        assert_eq!(
            derive(Method::Mnemonic, PHRASE, "", 0).unwrap(),
            PHRASE_ADDR
        );
    }

    #[test]
    fn the_wallet_index_selects_a_different_account() {
        let first = derive(Method::Mnemonic, PHRASE, "", 0).unwrap();
        let second = derive(Method::Mnemonic, PHRASE, "", 1).unwrap();
        assert_ne!(first, second, "index must walk the BIP-32 tree");
    }

    /// Addresses produced by ethers.js v6 for the same inputs, transcribed here
    /// so the derivation is pinned against an *independent* implementation
    /// rather than only against itself. Regenerate with:
    ///
    /// ```text
    /// new ethers.Wallet(key).address
    /// ethers.HDNodeWallet.fromPhrase(phrase, '', "m/44'/60'/0'/0/{i}").address
    /// ```
    #[test]
    fn derivation_matches_ethers_js() {
        let cases: [(Method, &str, &str, u32, &str); 4] = [
            (Method::Mnemonic, PHRASE, "", 0, PHRASE_ADDR),
            (
                Method::Mnemonic,
                PHRASE,
                "",
                1,
                "0x6fac4d18c912343bf86fa7049364dd4e424ab9c0",
            ),
            (Method::PrivateKey, "", KEY_ONE, 0, KEY_ONE_ADDR),
            (
                Method::PrivateKey,
                "",
                "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
                0,
                "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23",
            ),
        ];

        for (method, phrase, key, index, expected) in cases {
            assert_eq!(
                derive(method, phrase, key, index).unwrap(),
                expected,
                "{method:?} at index {index} diverged from ethers.js"
            );
        }
    }

    #[test]
    fn a_private_key_derives_its_address() {
        assert_eq!(
            derive(Method::PrivateKey, "", KEY_ONE, 0).unwrap(),
            KEY_ONE_ADDR
        );
    }

    #[test]
    fn the_zero_x_prefix_is_optional_and_case_is_ignored() {
        let bare = derive(Method::PrivateKey, "", KEY_ONE, 0).unwrap();
        let prefixed = derive(Method::PrivateKey, "", &format!("0x{KEY_ONE}"), 0).unwrap();
        let upper = derive(Method::PrivateKey, "", &KEY_ONE.to_uppercase(), 0).unwrap();
        let spaced = derive(Method::PrivateKey, "", &format!("  0x{KEY_ONE}  "), 0).unwrap();

        assert_eq!(bare, prefixed);
        assert_eq!(bare, upper);
        assert_eq!(bare, spaced);
    }

    #[test]
    fn the_wallet_index_is_ignored_for_a_private_key() {
        // A raw key has no derivation tree; a stale index left over from the
        // other tab must not silently change which account you sign in as.
        for index in [0, 1, 7] {
            assert_eq!(
                derive(Method::PrivateKey, "", KEY_ONE, index).unwrap(),
                KEY_ONE_ADDR
            );
        }
    }

    #[test]
    fn a_phrase_that_fails_its_checksum_is_rejected() {
        // Accepting it would derive a wallet nobody owns, and the failure would
        // surface much later as "no such account".
        let bad = PHRASE.replace("about", "abandon");
        let err = derive(Method::Mnemonic, &bad, "", 0).unwrap_err();
        assert!(err.contains("checksum"), "unhelpful message: {err}");
    }

    #[test]
    fn a_malformed_private_key_says_what_is_wrong_with_it() {
        let not_hex = derive(Method::PrivateKey, "", "0xzzzz", 0).unwrap_err();
        assert!(not_hex.contains("hexadecimal"), "got: {not_hex}");

        let too_short = derive(Method::PrivateKey, "", "0xdeadbeef", 0).unwrap_err();
        assert!(too_short.contains("64"), "got: {too_short}");

        let too_long = derive(Method::PrivateKey, "", &format!("{KEY_ONE}00"), 0).unwrap_err();
        assert!(too_long.contains("64"), "got: {too_long}");
    }

    #[test]
    fn out_of_range_scalars_are_rejected() {
        // 0 and n are both well-formed hex and both unusable as keys.
        let zero = "0".repeat(64);
        assert!(derive(Method::PrivateKey, "", &zero, 0).is_err());

        let curve_order = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";
        assert!(derive(Method::PrivateKey, "", curve_order, 0).is_err());
    }

    #[test]
    fn an_empty_credential_names_the_field_it_wants() {
        assert!(derive(Method::Mnemonic, "   ", "", 0)
            .unwrap_err()
            .contains("recovery phrase"));
        assert!(derive(Method::PrivateKey, "", "  ", 0)
            .unwrap_err()
            .contains("private key"));
    }

    #[test]
    fn the_two_methods_do_not_read_each_others_fields() {
        // Both fields are populated; each method must use only its own, or
        // switching tabs would sign in as the wrong account.
        assert_eq!(
            derive(Method::Mnemonic, PHRASE, KEY_ONE, 0).unwrap(),
            PHRASE_ADDR
        );
        assert_eq!(
            derive(Method::PrivateKey, PHRASE, KEY_ONE, 0).unwrap(),
            KEY_ONE_ADDR
        );
    }

    #[test]
    fn each_method_has_its_own_tab_id_for_aria_labelling() {
        assert_ne!(Method::Mnemonic.tab_id(), Method::PrivateKey.tab_id());
    }
}
