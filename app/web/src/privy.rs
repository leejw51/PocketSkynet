//! Privy sign-in, via the bridge in `web/privy/bridge.jsx`.
//!
//! Privy has no Rust SDK and no framework-agnostic build in this tree — it ships
//! as React hooks. So the bundle at `static/vendor/privy/privy.js` is the
//! smallest React app that can exist, and its only export is an imperative
//! `window.psPrivy`. This module is the Rust half of that conversation.
//!
//! **The bundle is loaded on demand, never at startup.** It is 4.3 MB (1.3 MB
//! over the wire) because it carries React, react-dom and the whole Privy SDK,
//! and this client's premise is cold-start speed. Making every sign-in pay 1.3 MB
//! so that some sign-ins can use Privy would be the wrong trade; the script tag
//! is injected the moment someone chooses Privy and not before.
//!
//! **What comes back is an EIP-1193 provider.** The bridge resolves Privy's
//! embedded wallet to `getEthereumProvider()`, so from here on Privy is
//! indistinguishable from MetaMask and goes through the same
//! [`crate::eip1193::Provider`] — same challenge signature, same key derivation,
//! same binding. That is deliberate: a second derivation path is how two clients
//! end up disagreeing about who someone is.
//!
//! The server never talks to Privy. It sees an ordinary EIP-191 signature from
//! an EOA and cannot tell which wallet produced it, exactly as in the reference
//! client — there is no Privy token verification anywhere.

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::eip1193::{Provider, WalletError};

/// Where the vendored bundle lives. Same-origin and checked in: nothing is
/// fetched from a CDN, so this works on an air-gapped LAN and inside a packaged
/// binary.
const BUNDLE: &str = "/static/vendor/privy/privy.js";

/// `window.psPrivy`, or `None` when the bundle has not loaded yet.
fn bridge() -> Option<JsValue> {
    let win = web_sys::window()?;
    let b = js_sys::Reflect::get(&win, &"psPrivy".into()).ok()?;
    (!b.is_undefined() && !b.is_null()).then_some(b)
}

/// Load the bundle once, resolving when `window.psPrivy` exists.
///
/// Idempotent: a second call with the script already present resolves
/// immediately rather than injecting a duplicate tag.
async fn load_bundle() -> Result<JsValue, WalletError> {
    if let Some(b) = bridge() {
        return Ok(b);
    }

    let win = web_sys::window().ok_or(WalletError::NotInstalled)?;
    let doc = win.document().ok_or(WalletError::NotInstalled)?;

    // Reuse an in-flight tag rather than adding a second one: two clicks in
    // quick succession must not download 4 MB twice.
    let existing = doc
        .query_selector(&format!("script[src='{BUNDLE}']"))
        .ok()
        .flatten();
    if existing.is_none() {
        let el = doc
            .create_element("script")
            .map_err(|_| WalletError::Protocol("could not create script".into()))?;
        let _ = el.set_attribute("src", BUNDLE);
        // Not `async`: the bundle defines a global that everything below waits
        // on, and ordering is easier to reason about than a load event race.
        let _ = el.set_attribute("data-ps-privy", "1");
        doc.body()
            .ok_or(WalletError::NotInstalled)?
            .append_child(&el)
            .map_err(|_| WalletError::Protocol("could not add script".into()))?;
    }

    // Poll for the global. A `load` listener would be tidier, but the script may
    // already be loading from a previous click, in which case its load event has
    // no listener left to fire at.
    for _ in 0..400 {
        if let Some(b) = bridge() {
            return Ok(b);
        }
        sleep(50).await;
    }
    Err(WalletError::Protocol("privy bundle did not load".into()))
}

async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

/// Call a method on the bridge and await its promise.
async fn call(name: &str, args: &js_sys::Array) -> Result<JsValue, WalletError> {
    let b = bridge().ok_or(WalletError::NotInstalled)?;
    let f = js_sys::Reflect::get(&b, &name.into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| WalletError::Protocol(format!("privy bridge has no {name}")))?;

    let out =
        js_sys::Reflect::apply(&f, &b, args).map_err(|e| WalletError::Protocol(describe(&e)))?;

    // Sync methods return a value; async ones return a promise. Accept both so
    // the bridge is free to change which is which.
    match out.dyn_into::<js_sys::Promise>() {
        Ok(p) => JsFuture::from(p).await.map_err(|e| map_rejection(&e)),
        Err(v) => Ok(v),
    }
}

/// The bridge rejects with plain `Error`s whose message is written for a person
/// ("Sign-in was not completed"). Those are worth showing; anything else is not.
fn map_rejection(e: &JsValue) -> WalletError {
    classify(&describe(e))
}

/// Split out from [`map_rejection`] so it can be tested without a JS engine:
/// touching a `JsValue` on the host target aborts the process rather than
/// failing, so anything under test has to be plain Rust.
fn classify(msg: &str) -> WalletError {
    if msg.contains("not completed") || msg.contains("cancel") {
        WalletError::Rejected
    } else {
        WalletError::Protocol(msg.to_owned())
    }
}

fn describe(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|m| m.as_string())
        .or_else(|| e.as_string())
        .unwrap_or_else(|| "privy request failed".to_owned())
}

/// Chain details handed to Privy so the wallet it creates is on the same chain
/// the rest of the app talks to.
pub struct Chain {
    pub id: u64,
    pub name: String,
    pub rpc: String,
    pub explorer: String,
}

/// Load the bundle, mount the provider, open the modal, and return an EIP-1193
/// provider for the embedded wallet.
///
/// One call, one `.await`. The reference client makes this two clicks — the first
/// opens the modal and returns — but from here the whole thing is a single
/// future, and asking someone to press the same button twice is a worse
/// interface rather than a simpler one.
pub async fn connect(app_id: &str, chain: Option<Chain>) -> Result<Provider, WalletError> {
    if app_id.trim().is_empty() {
        // Should be unreachable: the button is not offered without an id.
        return Err(WalletError::NotInstalled);
    }
    load_bundle().await?;

    // init(appId, chain)
    let chain_obj = match &chain {
        Some(c) => {
            let o = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&o, &"id".into(), &JsValue::from_f64(c.id as f64));
            let _ = js_sys::Reflect::set(&o, &"name".into(), &JsValue::from_str(&c.name));
            let _ = js_sys::Reflect::set(&o, &"rpc".into(), &JsValue::from_str(&c.rpc));
            let _ = js_sys::Reflect::set(&o, &"explorer".into(), &JsValue::from_str(&c.explorer));
            o.into()
        }
        None => JsValue::NULL,
    };
    let args = js_sys::Array::new();
    args.push(&JsValue::from_str(app_id));
    args.push(&chain_obj);
    let ok = call("init", &args).await?;
    if ok.as_bool() == Some(false) {
        return Err(WalletError::Protocol("privy refused to initialise".into()));
    }

    // connect() → address. Waits through the modal, the email code, and wallet
    // creation, so this future can be open for minutes.
    let address = call("connect", &js_sys::Array::new()).await?;
    let address = address
        .as_string()
        .ok_or_else(|| WalletError::Protocol("privy returned no address".into()))?;

    // provider(address) → EIP-1193. From here, Privy is MetaMask.
    let args = js_sys::Array::new();
    args.push(&JsValue::from_str(&address));
    let raw = call("provider", &args).await?;
    Provider::wrap(raw)
        .ok_or_else(|| WalletError::Protocol("privy did not return an EIP-1193 provider".into()))
}

/// Sign out of Privy as well as of this app.
///
/// Without this, "sign out" leaves the Privy session authenticated, and the next
/// sign-in silently reuses the previous account — which looks like the app
/// ignoring the sign-out.
pub async fn disconnect() {
    if bridge().is_some() {
        let _ = call("disconnect", &js_sys::Array::new()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundle_is_same_origin_and_never_a_cdn() {
        // The whole air-gap guarantee rests on this one string.
        assert!(BUNDLE.starts_with('/'), "{BUNDLE} must be same-origin");
        assert!(!BUNDLE.contains("//"), "{BUNDLE} must not be a URL");
        assert!(
            !BUNDLE.contains("http") && !BUNDLE.contains("cdn"),
            "{BUNDLE} must not be fetched from a third party"
        );
    }

    #[test]
    fn a_cancelled_modal_is_told_apart_from_a_broken_one() {
        // "You closed it" and "it is broken" need different copy. The bridge
        // signals the first with a message rather than an EIP-1193 code, since
        // closing a Privy modal is not a provider rejection.
        assert_eq!(classify("Sign-in was not completed"), WalletError::Rejected);
        assert_eq!(classify("user cancelled"), WalletError::Rejected);
        // Anything else keeps its text for the log and shows generic copy.
        assert!(matches!(
            classify("network unreachable"),
            WalletError::Protocol(_)
        ));
        assert_eq!(
            format!("{:?}", classify("network unreachable").key()),
            "wallet_failed"
        );
    }
}
