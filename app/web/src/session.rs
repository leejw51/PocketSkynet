//! Session identity, and the persistence policy.
//!
//! # What is written to `localStorage`, and why
//!
//! | Key | Contents | Reasoning |
//! |---|---|---|
//! | `ps-session` | JWT, wallet address, username | The JWT is a bearer credential the server issued and can be re-issued; storing it is what makes a reload not a sign-in. It grants **read** access to the account's rooms but, on its own, decrypts nothing. |
//! | `ps-theme` | `light` \| `dark` \| absent | Cosmetic. |
//! | `ps-skin` | `skynet` \| `cuteskynet` | Cosmetic; the art direction, independent of light/dark. Written before authentication like `ps-lang`, so the sign-in screen wears the chosen skin. |
//! | `ps-lang` | language tag | Cosmetic; deliberately written *before* authentication so the login screen itself remembers. |
//! | `ps-connection-mode` | `websocket` \| `sse` \| `polling` | A user preference about transport, not a secret. |
//! | `ps-login-layout` | `auto` \| `vertical` \| `horizontal` | Cosmetic; written before authentication, like `ps-lang`, so the sign-in screen itself remembers. |
//! | `ps-shell-layout` | `horizontal` \| `vertical` | Cosmetic. How the two panes sit on a wide viewport: beside each other, or list above chat. |
//! | `ps-cursor:<roomId>` | last synced `msgSerial` | A position, not content. Losing it costs one full re-sync. |
//! | `ps-cache:*` | room list, message rows **in wire form**, wrapped epoch keys | The persisted cache ([`crate::cache`]) — what makes reopening a room cost zero requests. Everything in it is ciphertext the server already stores for this account; plaintext and unwrapped keys never enter it. Cleared on sign-out. |
//! | `ps-wallet` | username + the sign-in credential | **Opt-in**, and the one entry here that is key material. See [`crate::vault`], which owns it — this module never touches it. |
//!
//! # What is **never** written anywhere
//!
//! The derived E2EE private key, the per-account derivation salt, and every
//! unwrapped room key. All of it lives in process memory for the lifetime of
//! the tab and is gone on reload.
//!
//! This is a deliberate trade against the reference web client, which keeps the
//! E2EE private key and decrypted room keys in `localStorage`. That storage is
//! readable by any script that achieves XSS, and a single injected script would
//! exfiltrate the key that decrypts *every epoch of every room, forever* —
//! including history — because the key is deterministically derived and never
//! rotates. Deriving them again from a credential costs one elliptic-curve
//! operation; storing them buys nothing and widens the blast radius. So they
//! are re-derived, never stored.
//!
//! The consequence is a third auth state, which the login screen implements:
//! **locked** — a valid JWT with no keys in memory. Plaintext rooms are
//! readable; encrypted ones show sealed bubbles until the credential is
//! supplied — by the user, or by [`crate::vault`] if this device was told to
//! remember it.

use std::cell::RefCell;
use std::rc::Rc;

use pocketskynet_core::WalletAddress;
use serde::{Deserialize, Serialize};

use crate::api::User;
use crate::crypto::SessionKeys;

const KEY_SESSION: &str = "ps-session";
const KEY_THEME: &str = "ps-theme";
const KEY_SKIN: &str = "ps-skin";
const KEY_CONNECTION: &str = "ps-connection-mode";
const KEY_LOGIN_LAYOUT: &str = "ps-login-layout";
const KEY_SHELL_LAYOUT: &str = "ps-shell-layout";
const KEY_FONT: &str = "ps-font";
const KEY_FONT_SCALE: &str = "ps-font-scale";
const KEY_CURSOR_PREFIX: &str = "ps-cursor:";

/// `localStorage` access, isolated behind four functions.
///
/// The `cfg` split is what lets every pure test in this crate run on the host:
/// `web_sys`/`js_sys` globals panic outside a browser, so on a non-wasm target
/// these degrade to a no-op store. Nothing else in the crate touches
/// `localStorage` directly.
pub(crate) mod backend {
    #[cfg(target_arch = "wasm32")]
    pub use imp::*;

    #[cfg(target_arch = "wasm32")]
    mod imp {
        use gloo_storage::{LocalStorage, Storage};
        use serde::de::DeserializeOwned;
        use serde::Serialize;

        pub fn get<T: DeserializeOwned>(key: &str) -> Option<T> {
            LocalStorage::get(key).ok()
        }
        pub fn set<T: Serialize>(key: &str, value: &T) {
            // A quota or private-mode failure must never break the app; the
            // only consequence is that the preference does not survive a reload.
            let _ = LocalStorage::set(key, value);
        }
        pub fn delete(key: &str) {
            LocalStorage::delete(key);
        }
        pub fn clear() {
            LocalStorage::clear();
        }
        pub fn root_element() -> Option<web_sys::Element> {
            web_sys::window()?.document()?.document_element()
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub use stub::*;

    #[cfg(not(target_arch = "wasm32"))]
    mod stub {
        #![allow(dead_code)]

        use serde::de::DeserializeOwned;
        use serde::Serialize;

        pub fn get<T: DeserializeOwned>(_key: &str) -> Option<T> {
            None
        }
        pub fn set<T: Serialize>(_key: &str, _value: &T) {}
        pub fn delete(_key: &str) {}
        pub fn clear() {}
        pub fn root_element() -> Option<()> {
            None
        }
    }
}

/// The part of a session that survives a reload. Note what is *not* here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSession {
    pub token: String,
    pub wallet_address: WalletAddress,
    pub username: String,
    /// The chosen avatar, carried so the profile card wears the right face
    /// between a reload and the first profile fetch. `default` because
    /// sessions persisted before the field existed must still deserialize.
    #[serde(default)]
    pub profile_image: Option<String>,
}

impl PersistedSession {
    pub fn load() -> Option<Self> {
        backend::get(KEY_SESSION)
    }

    pub fn save(&self) {
        backend::set(KEY_SESSION, self);
    }

    pub fn clear() {
        backend::delete(KEY_SESSION);
    }
}

/// A fully unlocked session: a token *and* the keys to use it with.
///
/// `keys` is behind `Rc<RefCell<…>>` because legacy-key derivation mutates
/// (memoises) the session, and because components need shared access without
/// cloning key material.
#[derive(Clone)]
pub struct Session {
    pub token: String,
    pub user: User,
    pub keys: Rc<RefCell<SessionKeys>>,
    /// The server's wallet: the required recipient of the on-chain transaction
    /// that anchors a message hash. Carried from the login response so the
    /// publish flow never has to trust an address supplied later.
    #[allow(dead_code)]
    pub fruitnation_wallet: String,
}

impl PartialEq for Session {
    /// Compared by identity, not by key material — `SessionKeys` is
    /// deliberately not comparable, and two sessions with the same token and
    /// user *are* the same session.
    fn eq(&self, other: &Self) -> bool {
        self.token == other.token && self.user.wallet_address == other.user.wallet_address
    }
}

impl Session {
    pub fn address(&self) -> &WalletAddress {
        &self.user.wallet_address
    }

    pub fn persist(&self) {
        PersistedSession {
            token: self.token.clone(),
            wallet_address: self.user.wallet_address.clone(),
            username: self.user.username.clone(),
            profile_image: self.user.profile_image.clone(),
        }
        .save();
    }
}

/// Where the app is, authentication-wise.
#[derive(Clone, PartialEq)]
pub enum Auth {
    /// No token. The login screen offers create/import.
    SignedOut,
    /// A stored token but no keys in memory — i.e. after a reload. Everything
    /// the JWT allows still works; encrypted content stays sealed.
    Locked(PersistedSession),
    /// Token plus keys.
    Unlocked(Session),
}

impl Auth {
    /// Load whatever survived the last page load. Never returns `Unlocked` —
    /// keys are never persisted, so unlocking always requires user input.
    pub fn restore() -> Self {
        match PersistedSession::load() {
            Some(s) if !s.token.is_empty() => Auth::Locked(s),
            _ => Auth::SignedOut,
        }
    }

    pub fn token(&self) -> Option<&str> {
        match self {
            Auth::SignedOut => None,
            Auth::Locked(p) => Some(&p.token),
            Auth::Unlocked(s) => Some(&s.token),
        }
    }

    pub fn address(&self) -> Option<&WalletAddress> {
        match self {
            Auth::SignedOut => None,
            Auth::Locked(p) => Some(&p.wallet_address),
            Auth::Unlocked(s) => Some(s.address()),
        }
    }

    pub fn username(&self) -> Option<&str> {
        match self {
            Auth::SignedOut => None,
            Auth::Locked(p) => Some(&p.username),
            Auth::Unlocked(s) => Some(&s.user.username),
        }
    }

    /// The chosen avatar (`User.profileImage`), if any.
    pub fn profile_image(&self) -> Option<&str> {
        match self {
            Auth::SignedOut => None,
            Auth::Locked(p) => p.profile_image.as_deref(),
            Auth::Unlocked(s) => s.user.profile_image.as_deref(),
        }
    }

    /// Whether the shell (rather than the login screen) should render.
    pub fn is_authenticated(&self) -> bool {
        !matches!(self, Auth::SignedOut)
    }

    pub fn session(&self) -> Option<&Session> {
        match self {
            Auth::Unlocked(s) => Some(s),
            _ => None,
        }
    }

    /// Whether encryption is currently usable. A locked session can read and
    /// post plaintext but cannot decrypt or seal anything.
    pub fn can_decrypt(&self) -> bool {
        matches!(self, Auth::Unlocked(_))
    }
}

/// Colour scheme preference (DESIGN.md §2.1). Absent means "follow the OS".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    /// Dark unless the user has chosen otherwise.
    ///
    /// This app is dark-first by design, not by following the OS: the product
    /// is a dark surface with one warm accent, and it is what someone sees on
    /// their first visit before they have any stored preference. `System` is
    /// still selectable and still honoured — it is simply not the default.
    pub fn load() -> Self {
        match backend::get::<String>(KEY_THEME).as_deref() {
            Some("light") => Theme::Light,
            Some("dark") => Theme::Dark,
            Some("system") => Theme::System,
            _ => Theme::Dark,
        }
    }

    /// Persist and apply. Applying is a single attribute on `<html>`; there is
    /// no second stylesheet and no re-render.
    pub fn apply(self) {
        match self {
            Theme::System => backend::set(KEY_THEME, &"system"),
            Theme::Light => backend::set(KEY_THEME, &"light"),
            Theme::Dark => backend::set(KEY_THEME, &"dark"),
        }
        #[cfg(target_arch = "wasm32")]
        if let Some(root) = backend::root_element() {
            match self {
                Theme::System => {
                    let _ = root.remove_attribute("data-theme");
                }
                Theme::Light => {
                    let _ = root.set_attribute("data-theme", "light");
                }
                Theme::Dark => {
                    let _ = root.set_attribute("data-theme", "dark");
                }
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

/// The art direction. Orthogonal to [`Theme`], and that separation is the
/// whole design: a skin decides *what the product looks like* — palette,
/// radii, type, imagery — while the theme decides only *how bright the room
/// is*. Every skin therefore has to work in light and in dark, which is why
/// this is a second attribute rather than two more entries in `Theme`.
///
/// Applied as `data-skin` on `<html>`, the same one-attribute mechanism the
/// theme and the font use. `Skynet` is the default and writes no attribute at
/// all, so a fresh install renders byte-identically to the sheet's `:root`
/// block and the skin costs nothing until someone chooses one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Skin {
    /// Machine cinema: near-black, optic cyan, squared geometry. §1 of app.css.
    #[default]
    Skynet,
    /// The same product drawn as a friendly mecha: primary blue, visor gold,
    /// signal red, generous radii, soft shadows. §1b of app.css.
    Cute,
}

impl Skin {
    pub const ALL: [Skin; 2] = [Skin::Skynet, Skin::Cute];

    pub fn load() -> Self {
        match backend::get::<String>(KEY_SKIN).as_deref() {
            Some("cuteskynet") => Skin::Cute,
            _ => Skin::Skynet,
        }
    }

    /// The value `app.css` selects on, and the value stored. One string for
    /// both, so a rename cannot desynchronise the sheet from the store.
    pub fn as_str(self) -> &'static str {
        match self {
            Skin::Skynet => "skynet",
            Skin::Cute => "cuteskynet",
        }
    }

    /// The directory under `static/img/` this skin's overrides live in.
    /// `None` for the default skin, whose art *is* the bare directory.
    pub fn art_dir(self) -> Option<&'static str> {
        match self {
            Skin::Skynet => None,
            Skin::Cute => Some("cute"),
        }
    }

    /// Persist and apply. The default removes the attribute, so nothing on a
    /// fresh install advertises a skin that is only the baseline.
    pub fn apply(self) {
        backend::set(KEY_SKIN, &self.as_str());
        #[cfg(target_arch = "wasm32")]
        if let Some(root) = backend::root_element() {
            if self == Skin::Skynet {
                let _ = root.remove_attribute("data-skin");
            } else {
                let _ = root.set_attribute("data-skin", self.as_str());
            }
        }
    }
}

/// The interface typeface. `System` is the platform stack the app ships
/// with; `Skynet` promotes the display face (Chakra Petch) to running text;
/// `Mono` and `Serif` are for people who read better in them. Applied as a
/// `data-font` attribute the same way the theme is applied — one attribute,
/// no second stylesheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontFace {
    #[default]
    System,
    Skynet,
    Mono,
    Serif,
}

impl FontFace {
    pub fn load() -> Self {
        match backend::get::<String>(KEY_FONT).as_deref() {
            Some("skynet") => FontFace::Skynet,
            Some("mono") => FontFace::Mono,
            Some("serif") => FontFace::Serif,
            _ => FontFace::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FontFace::System => "system",
            FontFace::Skynet => "skynet",
            FontFace::Mono => "mono",
            FontFace::Serif => "serif",
        }
    }

    /// The next face in the cycle, for the top-bar button.
    pub fn next(self) -> Self {
        match self {
            FontFace::System => FontFace::Skynet,
            FontFace::Skynet => FontFace::Mono,
            FontFace::Mono => FontFace::Serif,
            FontFace::Serif => FontFace::System,
        }
    }

    /// Persist and apply. The default removes the attribute so fresh
    /// installs carry no marker.
    pub fn apply(self) {
        backend::set(KEY_FONT, &self.as_str());
        #[cfg(target_arch = "wasm32")]
        if let Some(root) = backend::root_element() {
            if self == FontFace::System {
                let _ = root.remove_attribute("data-font");
            } else {
                let _ = root.set_attribute("data-font", self.as_str());
            }
        }
    }
}

/// The interface text size. Scales `:root`'s `font-size`, which every type
/// token is in `rem` of — one attribute, the whole interface follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontScale {
    Compact,
    #[default]
    Standard,
    Large,
    XLarge,
}

impl FontScale {
    pub fn load() -> Self {
        match backend::get::<String>(KEY_FONT_SCALE).as_deref() {
            Some("compact") => FontScale::Compact,
            Some("large") => FontScale::Large,
            Some("xlarge") => FontScale::XLarge,
            _ => FontScale::Standard,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FontScale::Compact => "compact",
            FontScale::Standard => "standard",
            FontScale::Large => "large",
            FontScale::XLarge => "xlarge",
        }
    }

    /// The next size in the cycle, for the top-bar button.
    pub fn next(self) -> Self {
        match self {
            FontScale::Compact => FontScale::Standard,
            FontScale::Standard => FontScale::Large,
            FontScale::Large => FontScale::XLarge,
            FontScale::XLarge => FontScale::Compact,
        }
    }

    pub fn apply(self) {
        backend::set(KEY_FONT_SCALE, &self.as_str());
        #[cfg(target_arch = "wasm32")]
        if let Some(root) = backend::root_element() {
            if self == FontScale::Standard {
                let _ = root.remove_attribute("data-fontsize");
            } else {
                let _ = root.set_attribute("data-fontsize", self.as_str());
            }
        }
    }
}

/// How the sign-in screen arranges its two panels — the form and the artwork.
///
/// `Auto` is the default and is what the CSS media queries decide: side by side
/// on a wide window, stacked on a narrow one. The other two override that,
/// because the breakpoint is a guess about the window and not about the person
/// in front of it — a wide window split in half leaves a narrow column of form,
/// and a stacked layout on a tall screen wastes the artwork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginLayout {
    /// Follow the viewport, as the stylesheet's breakpoints do.
    Auto,
    /// Artwork above, form below — one column, always.
    Vertical,
    /// Form beside artwork — two columns, always.
    Horizontal,
}

impl LoginLayout {
    pub fn load() -> Self {
        match backend::get::<String>(KEY_LOGIN_LAYOUT).as_deref() {
            Some("vertical") => LoginLayout::Vertical,
            Some("horizontal") => LoginLayout::Horizontal,
            _ => LoginLayout::Auto,
        }
    }

    pub fn save(self) {
        backend::set(KEY_LOGIN_LAYOUT, &self.as_str());
    }

    /// The value `app.css` matches on. `Auto` is the *absence* of the
    /// attribute, so the media queries are left to do their job rather than
    /// being overridden by a rule that says "behave normally".
    pub fn as_str(self) -> &'static str {
        match self {
            LoginLayout::Auto => "auto",
            LoginLayout::Vertical => "vertical",
            LoginLayout::Horizontal => "horizontal",
        }
    }

    pub fn attribute(self) -> Option<&'static str> {
        match self {
            LoginLayout::Auto => None,
            other => Some(other.as_str()),
        }
    }
}

/// How the two panes of the app shell sit on a viewport wide enough to show
/// both. Horizontal — list beside chat — is the messenger convention and the
/// default; vertical stacks the list above the conversation, which trades
/// visible history depth for full-width rows (and gives the rack (§6) the
/// whole shelf to wear its posters on). Below the two-pane breakpoint the
/// stylesheet ignores this entirely: a phone has one column either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellLayout {
    Horizontal,
    Vertical,
}

impl ShellLayout {
    pub fn load() -> Self {
        match backend::get::<String>(KEY_SHELL_LAYOUT).as_deref() {
            Some("vertical") => ShellLayout::Vertical,
            _ => ShellLayout::Horizontal,
        }
    }

    pub fn save(self) {
        backend::set(KEY_SHELL_LAYOUT, &self.as_str());
    }

    /// The value `.fn-panes[data-shell]` matches on.
    pub fn as_str(self) -> &'static str {
        match self {
            ShellLayout::Horizontal => "horizontal",
            ShellLayout::Vertical => "vertical",
        }
    }
}

/// The user's transport preference (REALTIME.md §7, §8.6).
///
/// This is the *preferred* tier. The realtime layer may degrade below it
/// automatically; it never silently upgrades past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    WebSocket,
    Sse,
    Polling,
}

impl ConnectionMode {
    pub fn load() -> Self {
        match backend::get::<String>(KEY_CONNECTION).as_deref() {
            Some("polling") => ConnectionMode::Polling,
            Some("sse") => ConnectionMode::Sse,
            // Unreadable storage defaults to WebSocket, matching the reference.
            _ => ConnectionMode::WebSocket,
        }
    }

    pub fn save(self) {
        backend::set(KEY_CONNECTION, &self.as_str());
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionMode::WebSocket => "websocket",
            ConnectionMode::Sse => "sse",
            ConnectionMode::Polling => "polling",
        }
    }
}

/// Load a room's persisted sync high-water mark. A position, never content.
pub fn load_cursor(room_id: &str) -> i64 {
    backend::get::<i64>(&format!("{KEY_CURSOR_PREFIX}{room_id}")).unwrap_or(0)
}

pub fn save_cursor(room_id: &str, serial: i64) {
    backend::set(&format!("{KEY_CURSOR_PREFIX}{room_id}"), &serial);
}

/// Erase every trace of this account from the device (Settings → Erase local
/// data). Deliberately clears **all** of `localStorage` rather than the keys we
/// know about: a key we forgot to list is exactly the one a user would want
/// gone. The wallet backup file the user downloaded is untouched — it is not
/// ours to delete.
pub fn erase_local_data() {
    backend::clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persisted() -> PersistedSession {
        PersistedSession {
            token: "jwt.token.value".into(),
            wallet_address: WalletAddress::new("0x742d35Cc6634C0532925a3b8D31cE5bb1C6E6B22")
                .unwrap(),
            username: "saltyOrchard42".into(),
            profile_image: None,
        }
    }

    #[test]
    fn the_persisted_shape_carries_no_key_material() {
        // This test is the enforcement mechanism for the policy in the module
        // docs: if someone adds a `mnemonic` or `encryption_key` field, the
        // serialised form gains a key and this fails.
        let json = serde_json::to_value(persisted()).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["profile_image", "token", "username", "wallet_address"]
        );

        // Field *names*, not values — a username may legitimately contain any
        // of these words.
        for forbidden in ["mnemonic", "private", "secret", "salt", "seed", "key"] {
            assert!(
                !keys.iter().any(|k| k.contains(forbidden)),
                "persisted session must not carry a {forbidden} field"
            );
        }
    }

    #[test]
    fn persisted_session_round_trips_and_normalises_the_address() {
        let json = serde_json::to_string(&persisted()).unwrap();
        let back: PersistedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back, persisted());
        assert_eq!(
            back.wallet_address.as_str(),
            "0x742d35cc6634c0532925a3b8d31ce5bb1c6e6b22"
        );
    }

    #[test]
    fn a_locked_session_is_authenticated_but_cannot_decrypt() {
        let auth = Auth::Locked(persisted());
        assert!(auth.is_authenticated());
        assert!(!auth.can_decrypt());
        assert_eq!(auth.token(), Some("jwt.token.value"));
        assert_eq!(auth.username(), Some("saltyOrchard42"));
        assert!(auth.session().is_none());
    }

    #[test]
    fn a_signed_out_session_exposes_nothing() {
        let auth = Auth::SignedOut;
        assert!(!auth.is_authenticated());
        assert!(!auth.can_decrypt());
        assert!(auth.token().is_none());
        assert!(auth.address().is_none());
    }

    #[test]
    fn connection_mode_round_trips_through_its_stored_string() {
        for m in [
            ConnectionMode::WebSocket,
            ConnectionMode::Sse,
            ConnectionMode::Polling,
        ] {
            let s = m.as_str();
            let parsed = match s {
                "polling" => ConnectionMode::Polling,
                "sse" => ConnectionMode::Sse,
                _ => ConnectionMode::WebSocket,
            };
            assert_eq!(parsed, m);
        }
    }

    #[test]
    fn a_first_visit_lands_on_dark() {
        // Dark-first is a product decision, not deference to the OS: the whole
        // surface is designed dark with one warm accent, and a first visit has
        // no stored preference to read. `System` remains selectable — it is
        // simply no longer what you get by default.
        assert_eq!(Theme::load(), Theme::Dark);
    }

    #[test]
    fn choosing_system_persists_rather_than_falling_back_to_dark() {
        // `System` used to be represented by the *absence* of a stored value,
        // which stopped working the moment absence came to mean Dark: picking
        // "System" would silently revert on the next load.
        assert_eq!(Theme::System.as_str(), "system");
        for t in [Theme::System, Theme::Light, Theme::Dark] {
            assert!(!t.as_str().is_empty(), "{t:?} must persist as a value");
        }
    }

    #[test]
    fn theme_strings_are_the_values_app_css_matches_on() {
        // `app.css` selects on `:root[data-theme="light"|"dark"]`; "system"
        // must never be written as an attribute value.
        assert_eq!(Theme::Light.as_str(), "light");
        assert_eq!(Theme::Dark.as_str(), "dark");
        assert_eq!(Theme::System.as_str(), "system");
    }

    // ---- skin (`ps-skin`) --------------------------------------------------

    #[test]
    fn skin_strings_are_the_values_app_css_matches_on() {
        // §1b selects on `:root[data-skin="cuteskynet"]`. If this string and
        // that selector ever disagree the app renders the base skin with the
        // picker insisting the other one is chosen — a silent failure, since
        // nothing errors and every asset still resolves.
        assert_eq!(Skin::Cute.as_str(), "cuteskynet");
        assert_eq!(Skin::Skynet.as_str(), "skynet");
    }

    #[test]
    fn the_default_skin_is_the_one_that_writes_no_attribute() {
        // With nothing stored, a fresh install must render byte-identically to
        // the app before skins existed — which means `Skynet`, whose `apply`
        // removes `data-skin` rather than setting it, so §1b never matches.
        assert_eq!(Skin::load(), Skin::Skynet);
        assert_eq!(Skin::default(), Skin::Skynet);
        assert!(Skin::Skynet.art_dir().is_none());
    }

    #[test]
    fn only_the_cute_skin_reaches_into_a_subdirectory() {
        // `asset::img` keys off `art_dir`, so a skin that claims a directory
        // it has no files in would render nothing at all.
        assert_eq!(Skin::Cute.art_dir(), Some("cute"));
    }

    #[test]
    fn every_skin_is_in_the_picker() {
        // Both pickers (login and Settings) iterate `ALL`; a variant missing
        // from it is a skin nobody can choose.
        assert_eq!(Skin::ALL.len(), 2);
        assert!(Skin::ALL.contains(&Skin::Skynet));
        assert!(Skin::ALL.contains(&Skin::Cute));
    }

    // ---- type preferences (`ps-font` / `ps-font-scale`) --------------------

    #[test]
    fn font_defaults_are_the_pre_preference_behaviour() {
        // With nothing stored (the host backend stores nothing), both load to
        // the variants that write no attribute — a fresh install must render
        // byte-identically to the app before these preferences existed.
        assert_eq!(FontFace::load(), FontFace::System);
        assert_eq!(FontScale::load(), FontScale::Standard);
        assert_eq!(FontFace::default(), FontFace::System);
        assert_eq!(FontScale::default(), FontScale::Standard);
    }

    #[test]
    fn the_font_cycles_visit_every_option_and_return_home() {
        // The topbar buttons step with `next()`; a variant `next()` skips
        // would be selectable in Settings but unreachable from the topbar.
        let mut face = FontFace::System;
        let mut seen = vec![face];
        for _ in 0..3 {
            face = face.next();
            assert!(!seen.contains(&face), "cycle revisited {face:?} early");
            seen.push(face);
        }
        assert_eq!(face.next(), FontFace::System, "the cycle must close");

        let mut scale = FontScale::Standard;
        let mut seen = vec![scale];
        for _ in 0..3 {
            scale = scale.next();
            assert!(!seen.contains(&scale), "cycle revisited {scale:?} early");
            seen.push(scale);
        }
        assert_eq!(scale.next(), FontScale::Standard, "the cycle must close");
    }

    #[test]
    fn font_strings_are_the_values_app_css_matches_on() {
        // `app.css` selects on `:root[data-font=…]` and `[data-fontsize=…]`;
        // these strings are the contract between Rust and the stylesheet.
        assert_eq!(FontFace::Skynet.as_str(), "skynet");
        assert_eq!(FontFace::Mono.as_str(), "mono");
        assert_eq!(FontFace::Serif.as_str(), "serif");
        assert_eq!(FontFace::System.as_str(), "system");
        assert_eq!(FontScale::Compact.as_str(), "compact");
        assert_eq!(FontScale::Standard.as_str(), "standard");
        assert_eq!(FontScale::Large.as_str(), "large");
        assert_eq!(FontScale::XLarge.as_str(), "xlarge");
    }
}
