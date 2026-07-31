//! Browser wallets, over EIP-1193.
//!
//! One thin async door onto `window.ethereum`. That is deliberately all it is:
//! the reference client (`server/client`) uses raw `window.ethereum.request`
//! for login rather than an ethers `BrowserProvider`, and there is nothing here
//! a provider abstraction would buy.
//!
//! **Why this exists at all.** PocketSkynet's E2EE identity is
//! `keccak256(personal_sign(derivation message))`, and the derivation and
//! binding messages in `core/src/keys.rs` are byte-identical to the reference
//! client's. So a wallet that can `personal_sign` produces *the same encryption
//! identity here as it does there* — a MetaMask user can sign in to either
//! client and read the same rooms. That interoperability is the whole point of
//! not inventing a second scheme.
//!
//! **What it cannot do.** The private key stays in the extension, so a session
//! signed in this way has no key to sign transactions with locally and no way
//! to derive the legacy unsalted key without a second wallet prompt on a
//! phishable message. Both are handled explicitly rather than silently — see
//! `crypto::Signer`.
//!
//! No EIP-6963 discovery: the reference client does not do it either, and
//! multi-provider disambiguation is a different feature from "log in with the
//! wallet in this browser".

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use pocketskynet_core::WalletAddress;

/// What went wrong, in terms a login screen can say out loud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletError {
    /// No injected provider. Not an error so much as an absence — the button
    /// that leads here should not have been offered.
    NotInstalled,
    /// The person declined the prompt. EIP-1193 code 4001.
    Rejected,
    /// The provider answered, but with something unusable.
    Protocol(String),
    /// No account came back, which happens when a wallet is locked.
    NoAccounts,
    /// The address the wallet gave is not one this app can use.
    BadAddress,
}

impl WalletError {
    /// The i18n key for this failure. Kept here so every call site reports the
    /// same thing, and so `Protocol`'s payload is never shown raw — a provider's
    /// internal error text is not a user-facing string.
    pub fn key(&self) -> crate::i18n::Key {
        use crate::i18n::Key;
        match self {
            Self::NotInstalled => Key::wallet_not_found,
            Self::Rejected => Key::wallet_rejected,
            Self::NoAccounts => Key::wallet_no_account,
            Self::BadAddress => Key::wallet_bad_address,
            Self::Protocol(_) => Key::wallet_failed,
        }
    }
}

/// EIP-1193's "user rejected request".
const CODE_USER_REJECTED: f64 = 4001.0;

/// One EIP-1193 provider.
///
/// A value rather than a global, which is the whole reason Privy needs no second
/// implementation of login: MetaMask's injected object and a Privy embedded
/// wallet's `getEthereumProvider()` are both just a thing with `.request()`, so
/// they share every line below — including, critically, the key derivation that
/// decides someone's E2EE identity. Two derivations is how two clients end up
/// disagreeing about who you are.
#[derive(Clone)]
pub struct Provider(JsValue);

impl Provider {
    /// Wrap an object that already satisfies EIP-1193 (Privy's bridge hands one
    /// over). Rejected if it has no callable `request`, so a wrong object fails
    /// here rather than at the first signature.
    pub fn wrap(obj: JsValue) -> Option<Self> {
        let ok = js_sys::Reflect::get(&obj, &"request".into())
            .ok()
            .is_some_and(|f| f.dyn_ref::<js_sys::Function>().is_some());
        ok.then_some(Self(obj))
    }

    /// The browser's injected provider, or `None`.
    #[cfg(target_arch = "wasm32")]
    pub fn injected() -> Option<Self> {
        let win = web_sys::window()?;
        let eth = js_sys::Reflect::get(&win, &JsValue::from_str("ethereum")).ok()?;
        if eth.is_undefined() || eth.is_null() {
            return None;
        }
        Self::wrap(eth)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn injected() -> Option<Self> {
        None
    }
}

/// Whether a browser wallet is present. Used to decide whether to *offer*
/// wallet sign-in at all: a button that always fails is worse than no button.
pub fn available() -> bool {
    Provider::injected().is_some()
}

/// Whether this looks like a phone or tablet.
///
/// Only ever used to decide *which* wallet affordance to show, never to gate a
/// capability — the provider check above is the authority on what actually
/// works. A wrong guess here shows a button that turns out to be unnecessary,
/// not a broken one.
#[cfg(target_arch = "wasm32")]
pub fn is_mobile() -> bool {
    let Some(win) = web_sys::window() else {
        return false;
    };
    let ua = win.navigator().user_agent().unwrap_or_default();
    let ua = ua.to_ascii_lowercase();
    // iPadOS reports itself as a Mac, so `Macintosh` plus touch points is the
    // only way to tell a tablet from a laptop.
    let ipad_pretending = ua.contains("macintosh")
        && js_sys::Reflect::get(&win.navigator(), &"maxTouchPoints".into())
            .ok()
            .and_then(|v| v.as_f64())
            .is_some_and(|n| n > 1.0);
    ipad_pretending
        || ["iphone", "ipad", "ipod", "android"]
            .iter()
            .any(|needle| ua.contains(needle))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn is_mobile() -> bool {
    false
}

/// A link that reopens this page inside MetaMask's own browser.
///
/// **This is the only way MetaMask can sign on iOS.** There, MetaMask is an app,
/// not a browser extension: Safari and Chrome inject no `window.ethereum`
/// whatsoever, so there is no provider to talk to and no amount of client code
/// changes that. MetaMask Mobile ships a built-in browser that *does* inject
/// one, and `metamask.app.link` is how a page asks to be opened in it.
///
/// The link deliberately carries the current host, port and path, so whichever
/// address someone reached this server on is the one MetaMask reopens.
///
/// One caveat this cannot solve: the deep link always resolves as **https**, so
/// a server running with a self-signed certificate will make MetaMask's browser
/// show a certificate warning it may refuse to let past. The way through is to
/// install this server's CA — the startup banner prints the `/ca.crt` URL and
/// the steps — after which the certificate is trusted system-wide and MetaMask's
/// browser accepts it like any other site.
#[cfg(target_arch = "wasm32")]
pub fn metamask_deeplink() -> Option<String> {
    let loc = web_sys::window()?.location();
    let host = loc.host().ok()?; // host:port
    if host.is_empty() {
        return None;
    }
    let path = loc.pathname().unwrap_or_else(|_| "/".into());
    // `metamask.app.link/dapp/<host><path>` — no scheme, by MetaMask's contract.
    Some(format!("https://metamask.app.link/dapp/{host}{path}"))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn metamask_deeplink() -> Option<String> {
    None
}

/// Call one EIP-1193 method.
///
/// `params` is a JS array. Errors are mapped rather than passed through, because
/// a provider's rejection object is not something to render.
async fn request_on(
    eth: &JsValue,
    method: &str,
    params: js_sys::Array,
) -> Result<JsValue, WalletError> {
    let eth = eth.clone();

    let arg = js_sys::Object::new();
    js_sys::Reflect::set(&arg, &"method".into(), &method.into())
        .map_err(|_| WalletError::Protocol("could not build request".into()))?;
    js_sys::Reflect::set(&arg, &"params".into(), &params)
        .map_err(|_| WalletError::Protocol("could not build params".into()))?;

    let func = js_sys::Reflect::get(&eth, &"request".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or(WalletError::NotInstalled)?;

    let promise = func
        .call1(&eth, &arg)
        .map_err(|e| WalletError::Protocol(describe(&e)))?
        .dyn_into::<js_sys::Promise>()
        .map_err(|_| WalletError::Protocol("request did not return a promise".into()))?;

    JsFuture::from(promise).await.map_err(|e| {
        // 4001 is the one code worth distinguishing: "you cancelled" and
        // "something is broken" call for different copy.
        let code = js_sys::Reflect::get(&e, &"code".into())
            .ok()
            .and_then(|c| c.as_f64());
        if code == Some(CODE_USER_REJECTED) {
            WalletError::Rejected
        } else {
            WalletError::Protocol(describe(&e))
        }
    })
}

/// A short description of a JS error, for logs only.
fn describe(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|m| m.as_string())
        .or_else(|| e.as_string())
        .unwrap_or_else(|| "wallet request failed".to_owned())
}

impl Provider {
    /// Prompt for account access and return the first account.
    ///
    /// The address is returned in the wallet's own casing, because that is what has
    /// to go into `personal_sign` — but every *wire* use lowercases it, which
    /// `WalletAddress` does on construction.
    pub async fn connect(&self) -> Result<(WalletAddress, String), WalletError> {
        let accounts = request_on(&self.0, "eth_requestAccounts", js_sys::Array::new()).await?;
        let list: js_sys::Array = accounts
            .dyn_into()
            .map_err(|_| WalletError::Protocol("accounts was not an array".into()))?;
        if list.length() == 0 {
            return Err(WalletError::NoAccounts);
        }
        let raw = list
            .get(0)
            .as_string()
            .ok_or(WalletError::Protocol("account was not a string".into()))?;
        let parsed = WalletAddress::new(&raw).map_err(|_| WalletError::BadAddress)?;
        Ok((parsed, raw))
    }

    /// Sign `message` as EIP-191 personal data.
    ///
    /// Parameter order is `[message, address]` — the modern `personal_sign` order,
    /// **not** the legacy `eth_sign` `[address, data]`. Getting this backwards
    /// produces either an error or, worse, a valid signature over the wrong bytes.
    ///
    /// `message` goes as plain UTF-8 text, matching the reference client. MetaMask
    /// then shows the person the actual words they are signing, which for a
    /// derivation message that *is* an encryption key is the difference between an
    /// informed action and a blind one.
    ///
    /// `address` must be the casing the wallet reported; some providers reject an
    /// address that does not match what they handed out.
    pub async fn personal_sign(
        &self,
        message: &str,
        address_as_given: &str,
    ) -> Result<String, WalletError> {
        let params = js_sys::Array::new();
        params.push(&JsValue::from_str(message));
        params.push(&JsValue::from_str(address_as_given));

        let sig = request_on(&self.0, "personal_sign", params).await?;
        let sig = sig
            .as_string()
            .ok_or(WalletError::Protocol("signature was not a string".into()))?;

        // 0x + 65 bytes. Checked here rather than at the first use, so a provider
        // that returns something odd fails at the door with a clear reason instead
        // of deep inside key derivation.
        let body = sig
            .strip_prefix("0x")
            .or_else(|| sig.strip_prefix("0X"))
            .ok_or(WalletError::Protocol(
                "signature was not 0x-prefixed".into(),
            ))?;
        if body.len() != 130 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(WalletError::Protocol("signature was malformed".into()));
        }
        Ok(sig)
    }

    /// The chain the wallet is currently on, as a decimal id.
    pub async fn chain_id(&self) -> Result<u64, WalletError> {
        let raw = request_on(&self.0, "eth_chainId", js_sys::Array::new()).await?;
        let hex = raw
            .as_string()
            .ok_or(WalletError::Protocol("chainId was not a string".into()))?;
        parse_chain_id(&hex).ok_or(WalletError::Protocol("chainId was not hex".into()))
    }

    /// Ask the wallet to switch chains. `Ok(false)` when the wallet does not know
    /// the chain (EIP-1193 code 4902) — the caller decides whether to offer to add
    /// it, which is a bigger ask than switching.
    pub async fn switch_chain(&self, chain_id: u64) -> Result<bool, WalletError> {
        let target = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &target,
            &"chainId".into(),
            &JsValue::from_str(&format!("0x{chain_id:x}")),
        );
        let params = js_sys::Array::new();
        params.push(&target);

        match request_on(&self.0, "wallet_switchEthereumChain", params).await {
            Ok(_) => Ok(true),
            Err(WalletError::Protocol(msg))
                if msg.contains("4902") || msg.contains("Unrecognized") =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

/// `"0x19"` → `25`. Tolerates a bare decimal string, which some wallets return
/// despite the spec.
fn parse_chain_id(raw: &str) -> Option<u64> {
    let t = raw.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => t.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_ids_parse_from_hex_and_from_a_bare_decimal() {
        assert_eq!(parse_chain_id("0x19"), Some(25));
        assert_eq!(parse_chain_id("0X152"), Some(338));
        assert_eq!(parse_chain_id("0x1"), Some(1));
        // Out of spec but seen in the wild.
        assert_eq!(parse_chain_id("338"), Some(338));
        assert_eq!(parse_chain_id("  0x19  "), Some(25));
        // Nothing usable.
        assert_eq!(parse_chain_id(""), None);
        assert_eq!(parse_chain_id("0x"), None);
        assert_eq!(parse_chain_id("zz"), None);
    }

    #[test]
    fn every_failure_has_its_own_message() {
        // A wallet failure the person cannot act on is a support ticket, so each
        // distinct cause must map to distinct copy rather than to one "error".
        use std::collections::HashSet;
        let keys: HashSet<_> = [
            WalletError::NotInstalled,
            WalletError::Rejected,
            WalletError::NoAccounts,
            WalletError::BadAddress,
            WalletError::Protocol("x".into()),
        ]
        .iter()
        .map(|e| format!("{:?}", e.key()))
        .collect();
        assert_eq!(keys.len(), 5, "two failures share one message: {keys:?}");
    }

    /// The deep-link shape, tested as a pure function so it can be checked
    /// without a browser. Kept separate from `metamask_deeplink` for exactly
    /// that reason.
    fn deeplink_for(host: &str, path: &str) -> Option<String> {
        if host.is_empty() {
            return None;
        }
        Some(format!("https://metamask.app.link/dapp/{host}{path}"))
    }

    #[test]
    fn the_deeplink_carries_the_address_this_server_was_reached_on() {
        // Whichever address someone used — loopback, LAN, VPN, with or without a
        // port — is the one MetaMask must reopen. Sending it to a different host
        // would land it on a server that is not this one.
        assert_eq!(
            deeplink_for("172.30.1.58:9099", "/login").unwrap(),
            "https://metamask.app.link/dapp/172.30.1.58:9099/login"
        );
        assert_eq!(
            deeplink_for("100.120.4.113:9099", "/").unwrap(),
            "https://metamask.app.link/dapp/100.120.4.113:9099/"
        );
        // No scheme in the path segment — that is MetaMask's contract, and
        // including one produces a link that silently opens nothing.
        let link = deeplink_for("example.test:9099", "/login").unwrap();
        assert!(!link.contains("dapp/https"), "{link}");
        assert!(!link.contains("dapp/http"), "{link}");
        // Nothing to open.
        assert_eq!(deeplink_for("", "/"), None);
    }

    #[test]
    fn a_provider_error_string_never_reaches_the_user() {
        // `Protocol` carries a provider's internal text for the log; the key it
        // maps to must be the generic one, not the payload.
        let e = WalletError::Protocol("insufficient funds for intrinsic gas".into());
        assert_eq!(format!("{:?}", e.key()), "wallet_failed");
    }
}
