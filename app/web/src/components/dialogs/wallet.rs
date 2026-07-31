//! The wallet: balances, active-network switcher, and Send funds
//! (DESIGN.md §10).
//!
//! The dialog is presided over by the **Vault Warden** (`wallet-warden.png`) —
//! a sentinel whose duty readout speaks the current phase and whose optic
//! ring scans, hurries, and locks with it. The avatar is where the dialog's
//! state is *said*; everything below it stays quiet chrome.
//!
//! One dialog, four phases — form, confirm, sending, receipt — modelled as a
//! single enum so the dialog can never show a receipt for a transaction it
//! has not sent. The confirmation is tiered like the reference client:
//! anything above 1 unit gets a warning banner, anything above 10 requires
//! retyping the exact amount.
//!
//! All chain traffic goes browser → RPC directly (`crate::rpc`); the server
//! is not involved in a send at all.

use pocketskynet_core::chain::{
    self, erc20_balance_of_data, erc20_transfer_data, format_amount, intrinsic_gas, parse_amount,
    ChainKind, LegacyTransaction, Network,
};
use pocketskynet_core::WalletAddress;
use web_sys::{HtmlInputElement, HtmlSelectElement};
use yew::prelude::*;

use crate::rpc::EvmRpc;
use crate::state::use_store;

use super::super::common::BusyButton;
use super::super::modal::Modal as Dialog;
use super::super::toast;
use crate::i18n::{t, Key, Lang};

/// Fallback gas price when `eth_gasPrice` fails: 2500 gwei, the constant the
/// reference client uses for Cronos.
const FALLBACK_GAS_PRICE: u128 = 2_500_000_000_000;

/// Fallback gas limit for an ERC-20 transfer when `eth_estimateGas` fails.
const ERC20_GAS_FALLBACK: u128 = 100_000;

#[derive(Properties, PartialEq)]
pub struct WalletProps {
    pub on_close: Callback<()>,
}

/// Which asset the send form is denominated in.
#[derive(Clone, PartialEq)]
struct Asset {
    symbol: String,
    decimals: u8,
    /// `None` = the chain's native token; `Some` = an ERC-20 contract.
    contract: Option<String>,
}

fn assets_of(net: &Network) -> Vec<Asset> {
    let mut out = vec![Asset {
        symbol: net.symbol.clone(),
        decimals: net.decimals,
        contract: None,
    }];
    // Server-registered tokens only: the wallet operates strictly on the
    // deployment's configured chain and registry. The Bank dialog is the
    // universal surface with its own networks and token imports.
    out.extend(net.tokens.iter().map(|t| Asset {
        symbol: t.symbol.clone(),
        decimals: t.decimals,
        contract: Some(t.contract.clone()),
    }));
    out
}

/// The wallet's two menus: what you have, and moving it. Anything more
/// advanced (other networks, swaps, contracts) lives in the Bank dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalletMenu {
    Balance,
    Send,
}

/// The dialog's phase.
#[derive(Clone, PartialEq)]
enum Phase {
    Form,
    /// Reviewing; `retyped` is the big-amount confirmation field.
    Confirm {
        retyped: String,
    },
    Sending,
    Receipt(Box<Outcome>),
}

#[derive(Clone, PartialEq)]
struct Outcome {
    ok: bool,
    detail: String,
    tx_hash: Option<String>,
    explorer: Option<String>,
    gas_used: Option<u128>,
    balance_before: Option<u128>,
    balance_after: Option<u128>,
    symbol: String,
    decimals: u8,
}

#[function_component(Wallet)]
pub fn wallet(p: &WalletProps) -> Html {
    let store = use_store();

    let lang = store.language;
    let net = store.active_network().cloned();
    let me = store.auth.address().cloned();

    // Balances, indexed like `assets_of`: [native, token, token…].
    let balances = use_state(|| Option::<Vec<Option<u128>>>::None);
    let phase = use_state(|| Phase::Form);
    // The two menus. Balance first: the question a wallet answers most.
    let menu = use_state(|| WalletMenu::Balance);

    // Send form fields.
    let asset_ix = use_state(|| 0usize);
    let to = use_state(String::new);
    let amount = use_state(String::new);
    let advanced = use_state(|| false);
    let gas_price_gwei = use_state(String::new);
    let gas_limit = use_state(String::new);
    let data_hex = use_state(String::new);
    let estimated_price = use_state(|| Option::<u128>::None);
    let error = use_state(|| Option::<String>::None);

    let assets = net.as_ref().map(assets_of).unwrap_or_default();
    let asset = assets.get(*asset_ix).cloned();
    let sendable = net.as_ref().is_some_and(Network::supports_send);

    // ---- balances -------------------------------------------------------

    let refresh_balances = {
        let balances = balances.clone();
        let net = net.clone();
        let me = me.clone();
        Callback::from(move |_: ()| {
            let (Some(net), Some(me)) = (net.clone(), me.clone()) else {
                return;
            };
            if !matches!(net.kind, ChainKind::Evm) {
                balances.set(Some(Vec::new()));
                return;
            }
            let balances = balances.clone();
            balances.set(None);
            wasm_bindgen_futures::spawn_local(async move {
                let rpc = EvmRpc::new(&net.rpc_url);
                let mut out = Vec::new();
                out.push(rpc.balance(&me).await.ok());
                // Iterate the same list `assets_of` builds, so the vector
                // stays index-aligned with what the menus render.
                for asset in assets_of(&net).into_iter().skip(1) {
                    let Some(contract) = &asset.contract else {
                        continue;
                    };
                    let data = format!("0x{}", hex::encode(erc20_balance_of_data(&me)));
                    let value = match rpc.eth_call(contract, &data).await {
                        Ok(hex_out) => chain::decode_abi_uint(&hex_out).ok(),
                        Err(_) => None,
                    };
                    out.push(value);
                }
                balances.set(Some(out));
            });
        })
    };

    // Load balances on open and whenever the active network changes.
    {
        let refresh = refresh_balances.clone();
        let net_id = net.as_ref().map(|n| n.id.clone());
        use_effect_with(net_id, move |_| {
            refresh.emit(());
        });
    }

    // ---- fee estimation --------------------------------------------------

    let estimate = {
        let net = net.clone();
        let me = me.clone();
        let asset = asset.clone();
        let to = to.clone();
        let amount = amount.clone();
        let data_hex = data_hex.clone();
        let gas_price_gwei = gas_price_gwei.clone();
        let gas_limit = gas_limit.clone();
        let estimated_price = estimated_price.clone();
        let error = error.clone();
        Callback::from(move |_: MouseEvent| {
            let (Some(net), Some(me), Some(asset)) = (net.clone(), me.clone(), asset.clone())
            else {
                return;
            };
            let gas_price_gwei = gas_price_gwei.clone();
            let gas_limit = gas_limit.clone();
            let estimated_price = estimated_price.clone();
            let error = error.clone();
            let to = (*to).clone();
            let amount_text = (*amount).clone();
            let data = (*data_hex).clone();
            wasm_bindgen_futures::spawn_local(async move {
                let rpc = EvmRpc::new(&net.rpc_url);
                let price = rpc.gas_price().await.unwrap_or(FALLBACK_GAS_PRICE);
                estimated_price.set(Some(price));
                gas_price_gwei.set(format_amount(price, 9));

                let limit = match &asset.contract {
                    None => u128::from(intrinsic_gas(&decode_data(&data).unwrap_or_default())),
                    Some(contract) => {
                        // A real node estimate, because contract execution
                        // cost is not knowable client-side. Requires a valid
                        // recipient and amount; fall back when absent.
                        match (
                            WalletAddress::new(&to),
                            parse_amount(&amount_text, asset.decimals),
                        ) {
                            (Ok(recipient), Ok(units)) => {
                                let calldata = format!(
                                    "0x{}",
                                    hex::encode(erc20_transfer_data(&recipient, units))
                                );
                                rpc.estimate_gas(&me, contract, 0, &calldata)
                                    .await
                                    // Nodes answer the *exact* cost; headroom
                                    // for a fee-on-transfer token's extra work.
                                    .map(|g| g + g / 5)
                                    .unwrap_or(ERC20_GAS_FALLBACK)
                            }
                            _ => ERC20_GAS_FALLBACK,
                        }
                    }
                };
                gas_limit.set(limit.to_string());
                error.set(None);
            });
        })
    };

    // ---- form → confirm --------------------------------------------------

    let review = {
        let phase = phase.clone();
        let error = error.clone();
        let asset = asset.clone();
        let to = to.clone();
        let amount = amount.clone();
        let balances = balances.clone();
        let asset_ix = asset_ix.clone();
        Callback::from(move |_: MouseEvent| {
            let Some(asset) = asset.clone() else { return };
            if WalletAddress::new(to.trim()).is_err() {
                error.set(Some("Enter a valid 0x recipient address.".into()));
                return;
            }
            let units = match parse_amount(&amount, asset.decimals) {
                Ok(0) => {
                    error.set(Some("Enter an amount above zero.".into()));
                    return;
                }
                Ok(u) => u,
                Err(e) => {
                    error.set(Some(
                        t(lang, Key::amount_error).replace("{error}", &e.to_string()),
                    ));
                    return;
                }
            };
            if let Some(Some(available)) = balances
                .as_ref()
                .map(|b| b.get(*asset_ix).copied().flatten())
            {
                if units > available {
                    error.set(Some(format!(
                        "That's more than your {} balance of {}.",
                        asset.symbol,
                        format_amount(available, asset.decimals)
                    )));
                    return;
                }
            }
            error.set(None);
            phase.set(Phase::Confirm {
                retyped: String::new(),
            });
        })
    };

    // ---- confirm → send --------------------------------------------------

    let send = {
        let store = store.clone();
        let phase = phase.clone();
        let net = net.clone();
        let me = me.clone();
        let asset = asset.clone();
        let to = to.clone();
        let amount = amount.clone();
        let data_hex = data_hex.clone();
        let gas_price_gwei = gas_price_gwei.clone();
        let gas_limit = gas_limit.clone();
        let refresh_balances = refresh_balances.clone();
        Callback::from(move |_: MouseEvent| {
            let (Some(net), Some(_me), Some(asset)) = (net.clone(), me.clone(), asset.clone())
            else {
                return;
            };
            let Some(session) = store.auth.session() else {
                toast::error(
                    &store,
                    "Wallet locked",
                    Some("Unlock your wallet to sign transactions.".into()),
                );
                return;
            };
            if net.chain_id.is_none() {
                return;
            }
            let Ok(recipient) = WalletAddress::new(to.trim()) else {
                return;
            };
            let Ok(units) = parse_amount(&amount, asset.decimals) else {
                return;
            };

            let gas_price = parse_amount(gas_price_gwei.trim(), 9)
                .ok()
                .filter(|p| *p > 0)
                .unwrap_or(FALLBACK_GAS_PRICE);
            let user_limit: Option<u128> = gas_limit.trim().parse().ok();
            let data = decode_data(&data_hex).unwrap_or_default();

            phase.set(Phase::Sending);

            let phase = phase.clone();
            let store = store.clone();
            let keys = session.keys.clone();
            let refresh_balances = refresh_balances.clone();
            let me = _me.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = run_send(
                    lang, &net, &me, keys, &asset, &recipient, units, gas_price, user_limit, data,
                )
                .await;
                if outcome.ok {
                    toast::success(&store, t(lang, Key::transaction_confirmed));
                    refresh_balances.emit(());
                }
                phase.set(Phase::Receipt(Box::new(outcome)));
            });
        })
    };

    // ---- render ----------------------------------------------------------

    let close = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: ()| on_close.emit(()))
    };
    let close_click = {
        let on_close = p.on_close.clone();
        Callback::from(move |_: MouseEvent| on_close.emit(()))
    };
    let busy = matches!(*phase, Phase::Sending);

    let Some(net) = net else {
        return html! {
            <Dialog title={t(lang, Key::wallet)} on_close={close} footer={None::<Html>}>
                <p>{ t(lang, Key::registry_not_loaded) }</p>
            </Dialog>
        };
    };

    // The Warden's duty status: one line per dialog phase, driving both the
    // readout text and the optic-ring state (`data-state` in app.css). The
    // avatar is not decoration — it is where the dialog's state is *said*.
    let scanning = balances.is_none();
    let (warden_state, warden_line) = match (*menu, &*phase) {
        (WalletMenu::Send, Phase::Confirm { .. }) => ("arm", t(lang, Key::warden_arm)),
        (WalletMenu::Send, Phase::Sending) => ("relay", t(lang, Key::warden_relay)),
        (WalletMenu::Send, Phase::Receipt(o)) if o.ok => ("ok", t(lang, Key::warden_ok)),
        (WalletMenu::Send, Phase::Receipt(_)) => ("fail", t(lang, Key::warden_fail)),
        (WalletMenu::Send, Phase::Form) => ("target", t(lang, Key::warden_target)),
        (WalletMenu::Balance, _) if scanning => ("scan", t(lang, Key::warden_scan)),
        (WalletMenu::Balance, _) => ("idle", t(lang, Key::warden_idle)),
    };

    let copy_addr = {
        let store = store.clone();
        let addr = me
            .as_ref()
            .map(WalletAddress::to_checksummed)
            .unwrap_or_default();
        Callback::from(move |_: MouseEvent| {
            if super::super::common::copy_to_clipboard(&addr) {
                toast::success(&store, t(store.language, Key::address_copied));
            }
        })
    };

    // Tap the warden → the spotlight stage, exactly like the AI Banker's
    // face. The stage carries the address as its quiet second line, and the
    // copy button inside it copies the same value the ID row does.
    let spotlight_warden = {
        let me = me.clone();
        Callback::from(move |_: MouseEvent| {
            super::super::spotlight::show(super::super::spotlight::Spot {
                image: "/static/img/wallet-warden.png".into(),
                title: t(lang, Key::warden_name).to_owned(),
                subtitle: me.as_ref().map(WalletAddress::to_checksummed),
                copy: me.as_ref().map(WalletAddress::to_checksummed),
                hue: 190,
            });
        })
    };

    // Amount parsed in display units for the confirmation tiers.
    let units_now = asset
        .as_ref()
        .and_then(|a| parse_amount(&amount, a.decimals).ok())
        .unwrap_or(0);
    let scale = asset
        .as_ref()
        .map(|a| 10u128.pow(a.decimals as u32))
        .unwrap_or(1);
    let over_one = units_now > scale;
    let over_ten = units_now > 10 * scale;

    let footer = match &*phase {
        Phase::Form => html! {
            <>
                <button type="button" class="topcoat-button" onclick={close_click.clone()}>
                    { t(lang, Key::close) }
                </button>
                if sendable {
                    <BusyButton label={t(lang, Key::review_send)} busy={false} onclick={review} />
                }
            </>
        },
        Phase::Confirm { retyped } => {
            let back = {
                let phase = phase.clone();
                Callback::from(move |_: MouseEvent| phase.set(Phase::Form))
            };
            let armed = !over_ten
                || asset
                    .as_ref()
                    .is_some_and(|a| parse_amount(retyped, a.decimals).ok() == Some(units_now));
            html! {
                <>
                    <button type="button" class="topcoat-button" onclick={back}>{ t(lang, Key::back) }</button>
                    <button
                        type="button"
                        class="topcoat-button--cta"
                        disabled={!armed}
                        onclick={send}
                    >
                        { t(lang, Key::send_amount).replace("{amount}", amount.trim())
                              .replace("{symbol}", asset.as_ref().map(|a| a.symbol.as_str()).unwrap_or("")) }
                    </button>
                </>
            }
        }
        Phase::Sending => html! {},
        Phase::Receipt(_) => html! {
            <button type="button" class="topcoat-button--cta" onclick={close_click.clone()}>
                { t(lang, Key::done) }
            </button>
        },
    };

    let menu_tab = |this: WalletMenu, label: Key, menu: &UseStateHandle<WalletMenu>| {
        let selected = **menu == this;
        let menu = menu.clone();
        html! {
            <button
                type="button"
                class="fn-tab"
                role="tab"
                aria-selected={selected.to_string()}
                onclick={Callback::from(move |_: MouseEvent| menu.set(this))}
            >{ t(lang, label) }</button>
        }
    };
    // The Send footer buttons only make sense inside the Send menu.
    let footer = match *menu {
        WalletMenu::Send => Some(footer),
        _ => None,
    };

    html! {
        <Dialog title={t(lang, Key::wallet)} busy={busy} wide=true on_close={close} {footer}>
            <div class="fn-wallet">
                <header class="fn-warden" data-state={warden_state}>
                    <button
                        type="button"
                        class="fn-spot__opener fn-warden__portrait"
                        aria-label={t(lang, Key::warden_name)}
                        onclick={spotlight_warden}
                    >
                        <span class="fn-warden__ring" aria-hidden="true"></span>
                        <img src="/static/img/wallet-warden.png" alt="" />
                    </button>
                    <div class="fn-warden__brief">
                        <span class="fn-warden__name">{ t(lang, Key::warden_name) }</span>
                        <p class="fn-warden__line" key={warden_state} role="status">
                            { warden_line }
                        </p>
                    </div>
                    { network_chip(lang, &net) }
                </header>

                if let Some(a) = &me {
                    <div class="fn-wallet__id">
                        <p class="fn-wallet__addr fn-nums">{ a.to_checksummed() }</p>
                        <button
                            type="button"
                            class="topcoat-icon-button--quiet"
                            aria-label={t(lang, Key::copy)}
                            title={t(lang, Key::copy)}
                            onclick={copy_addr}
                        >
                            { super::super::icons::copy(14) }
                        </button>
                    </div>
                }

                <div class="fn-tabs" role="tablist" aria-label={t(lang, Key::wallet)}>
                    { menu_tab(WalletMenu::Balance, Key::menu_balance, &menu) }
                    { menu_tab(WalletMenu::Send, Key::send, &menu) }
                </div>

                { match *menu {
                    WalletMenu::Balance => balances_view(lang, &net, &assets, &balances, &refresh_balances),
                    WalletMenu::Send => {
                        if !sendable {
                            html! {
                                <div class="fn-banner" role="note">
                                    { format!(
                                        "{} support is on the roadmap — switching works, sending doesn't yet.",
                                        net.name
                                    ) }
                                </div>
                            }
                        } else {
                            match &*phase {
                                Phase::Form => send_form(lang,
                                    &assets, &asset_ix, &to, &amount, &advanced, &gas_price_gwei,
                                    &gas_limit, &data_hex, &estimated_price, &error, estimate,
                                ),
                                Phase::Confirm { retyped } => confirm_view(lang,
                                    asset.as_ref(), &to, &amount, &gas_price_gwei, &gas_limit,
                                    over_one, over_ten, retyped, &phase,
                                ),
                                Phase::Sending => html! {
                                    <div class="fn-wallet__sending" role="status">
                                        <div class="fn-wallet__pulse" aria-hidden="true"></div>
                                        <p>{ t(lang, Key::broadcasting) }</p>
                                    </div>
                                },
                                Phase::Receipt(outcome) => receipt_view(lang, outcome),
                            }
                        }
                    }
                } }
            </div>
        </Dialog>
    }
}

/// Decode the optional data field: `0x…` passes through, anything else is
/// treated as UTF-8 text — the reference client's exact behaviour.
fn decode_data(input: &str) -> Option<Vec<u8>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    if let Some(stripped) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return hex::decode(stripped).ok();
    }
    Some(trimmed.as_bytes().to_vec())
}

/// Everything that happens after the user commits, in one testable-ish
/// sequence: chain check → nonce → sign → broadcast → poll receipt.
#[allow(clippy::too_many_arguments)]
async fn run_send(
    lang: Lang,
    net: &Network,
    me: &WalletAddress,
    keys: std::rc::Rc<std::cell::RefCell<crate::crypto::SessionKeys>>,
    asset: &Asset,
    recipient: &WalletAddress,
    units: u128,
    gas_price: u128,
    user_limit: Option<u128>,
    data: Vec<u8>,
) -> Outcome {
    let fail = |detail: String| Outcome {
        ok: false,
        detail,
        tx_hash: None,
        explorer: None,
        gas_used: None,
        balance_before: None,
        balance_after: None,
        symbol: asset.symbol.clone(),
        decimals: asset.decimals,
    };

    let Some(chain_id) = net.chain_id else {
        return fail(t(lang, Key::no_evm_chain_id).into());
    };
    let rpc = EvmRpc::new(&net.rpc_url);

    // The Skynet relay HUD (burst.rs). Raised here rather than around the
    // whole dialog so it covers exactly the on-chain span: chain check to
    // receipt. Ended on every return path via this guard-ish closure.
    use super::super::burst::{tx_end, tx_phase, tx_start, TxPhase};
    let hud = tx_start();
    tx_phase(hud, TxPhase::Uplink);
    let fail = |detail: String| {
        tx_end(hud, false);
        fail(detail)
    };

    // The registry says which chain this endpoint should be; believe the
    // endpoint only after it agrees. Signing with the wrong chain id doesn't
    // "fail" — it burns the fee on a tx the chain rejects.
    match rpc.chain_id().await {
        Ok(actual) if actual == u128::from(chain_id) => {}
        Ok(actual) => {
            return fail(format!(
                "RPC endpoint reports chain {actual}, expected {chain_id}. Not sending."
            ));
        }
        Err(e) => return fail(t(lang, Key::rpc_unreachable).replace("{error}", &e.to_string())),
    }

    let balance_before = rpc.balance(me).await.ok();
    let nonce = match rpc.nonce(me).await {
        Ok(n) => n,
        Err(e) => return fail(t(lang, Key::nonce_fetch_failed).replace("{error}", &e.to_string())),
    };

    // Native sends carry the value and the user's data; token sends carry
    // ERC-20 calldata to the contract with zero native value.
    let (tx_to, tx_value, tx_data) = match &asset.contract {
        None => (recipient.clone(), units, data),
        Some(contract) => {
            let Ok(contract_addr) = WalletAddress::new(contract) else {
                return fail(t(lang, Key::bad_token_address).into());
            };
            (contract_addr, 0, erc20_transfer_data(recipient, units))
        }
    };

    let gas_limit = user_limit
        .filter(|l| *l > 0)
        .unwrap_or_else(|| match &asset.contract {
            None => u128::from(intrinsic_gas(&tx_data)),
            Some(_) => ERC20_GAS_FALLBACK,
        });

    let tx = LegacyTransaction {
        nonce,
        gas_price,
        gas_limit,
        to: Some(tx_to),
        value: tx_value,
        data: tx_data,
        chain_id,
    };

    // Checked before signing rather than after failing: an external wallet has
    // no key on this device, and "signing failed: no signing key on this
    // device" is a dead end where a sentence about how to fix it belongs.
    if !keys.borrow().can_sign_locally() {
        return fail(t(lang, Key::wallet_no_local_key).to_owned());
    }
    tx_phase(hud, TxPhase::Sign);
    let signed = match keys.borrow().sign_transaction(&tx) {
        Ok(s) => s,
        Err(e) => return fail(t(lang, Key::signing_failed).replace("{error}", &e.to_string())),
    };

    tx_phase(hud, TxPhase::Broadcast);
    let tx_hash = match rpc.send_raw_transaction(&signed.raw_hex()).await {
        Ok(h) => h,
        Err(e) => {
            return fail(t(lang, Key::network_rejected_tx).replace("{error}", &e.to_string()))
        }
    };

    // Poll for the receipt. Cronos blocks are ~6s; give it ~60s before
    // declaring "pending" — which is not a failure, just not a receipt yet.
    tx_phase(hud, TxPhase::Confirm);
    let mut receipt = None;
    for _ in 0..20 {
        gloo_timers::future::TimeoutFuture::new(3_000).await;
        match rpc.receipt(&tx_hash).await {
            Ok(Some(r)) => {
                receipt = Some(r);
                break;
            }
            Ok(None) => continue,
            Err(_) => continue,
        }
    }

    let balance_after = rpc.balance(me).await.ok();
    tx_end(hud, receipt.as_ref().map(|r| r.ok).unwrap_or(true));
    match receipt {
        Some(r) => Outcome {
            ok: r.ok,
            detail: if r.ok {
                "Confirmed on chain.".into()
            } else {
                "The transaction was mined but reverted.".into()
            },
            tx_hash: Some(tx_hash.clone()),
            explorer: Some(net.tx_url(&tx_hash)),
            gas_used: Some(r.gas_used),
            balance_before,
            balance_after,
            symbol: asset.symbol.clone(),
            decimals: asset.decimals,
        },
        None => Outcome {
            ok: true,
            detail: "Broadcast accepted; still waiting for a receipt. Check the explorer.".into(),
            tx_hash: Some(tx_hash.clone()),
            explorer: Some(net.tx_url(&tx_hash)),
            gas_used: None,
            balance_before,
            balance_after,
            symbol: asset.symbol.clone(),
            decimals: asset.decimals,
        },
    }
}

// ---- sub-views ------------------------------------------------------------

/// The chain, stated rather than chosen.
///
/// This was a `<select>` over the whole registry. The chain is the
/// deployment's now — `VITE_CHAIN_ID` on the server, reported by
/// `GET /api/networks` — so there is nothing to pick, and a disabled dropdown
/// with one option would only invite the question of why it cannot be changed.
/// A pill in the Warden's header answers that by not asking it: a lit dot,
/// the chain's name, nothing to press.
fn network_chip(lang: Lang, net: &Network) -> Html {
    html! {
        <span class="fn-warden__net" title={t(lang, Key::network)}>
            { &net.name }
            if net.testnet {
                <span class="fn-badge fn-badge--muted">{ t(lang, Key::testnet_badge) }</span>
            }
        </span>
    }
}

/// A balance that tallies up to its value — the machine counting the vault,
/// not a spreadsheet cell appearing. Sixteen frames of eased approximation,
/// then it lands on the *exact* formatted amount; interpolating through `f64`
/// is fine for the frames because only the final, precise string survives.
/// Skipped entirely under `prefers-reduced-motion`.
#[derive(Properties, PartialEq)]
struct CountUpProps {
    value: u128,
    decimals: u8,
}

#[function_component(CountUp)]
fn count_up(p: &CountUpProps) -> Html {
    // `None` = settled on the exact value; `Some` = a mid-tally frame.
    let shown = use_state(|| Option::<String>::None);
    // A refresh mid-tally starts a new task; the generation stamp is how the
    // orphaned one knows to stop writing.
    let generation = use_mut_ref(|| 0u64);

    {
        let shown = shown.clone();
        let generation = generation.clone();
        use_effect_with((p.value, p.decimals), move |&(value, decimals)| {
            *generation.borrow_mut() += 1;
            let stamp = *generation.borrow();
            let reduce_motion = web_sys::window()
                .and_then(|w| {
                    w.match_media("(prefers-reduced-motion: reduce)")
                        .ok()
                        .flatten()
                })
                .is_some_and(|m| m.matches());
            if reduce_motion || value == 0 {
                shown.set(None);
            } else {
                wasm_bindgen_futures::spawn_local(async move {
                    const STEPS: u32 = 16;
                    for i in 1..STEPS {
                        if *generation.borrow() != stamp {
                            return;
                        }
                        let t = f64::from(i) / f64::from(STEPS);
                        let eased = 1.0 - (1.0 - t).powi(3);
                        let frame = (value as f64 * eased) as u128;
                        shown.set(Some(format_amount(frame.min(value), decimals)));
                        gloo_timers::future::TimeoutFuture::new(30).await;
                    }
                    if *generation.borrow() == stamp {
                        shown.set(None);
                    }
                });
            }
            || ()
        });
    }

    html! {
        <span class="fn-wallet__value fn-nums">
            { shown
                .as_ref()
                .cloned()
                .unwrap_or_else(|| format_amount(p.value, p.decimals)) }
        </span>
    }
}

fn balances_view(
    lang: Lang,
    net: &Network,
    assets: &[Asset],
    balances: &UseStateHandle<Option<Vec<Option<u128>>>>,
    refresh: &Callback<()>,
) -> Html {
    let refresh_click = {
        let refresh = refresh.clone();
        Callback::from(move |_: MouseEvent| refresh.emit(()))
    };
    html! {
        <div class="fn-wallet__balances">
            { for assets.iter().enumerate().map(|(i, a)| {
                let value = balances.as_ref().and_then(|b| b.get(i).copied());
                html! {
                    <div class="fn-wallet__balance" data-token={a.contract.is_some().to_string()}>
                        <span class="fn-bank__badge"
                              style={format!("--tok-h:{}", super::super::bank::token_hue(&a.symbol))}
                              aria-hidden="true">
                            { super::super::bank::token_monogram(&a.symbol) }
                        </span>
                        <span class="fn-wallet__symbol">{ &a.symbol }</span>
                        { match value {
                            None => html! { <span class="fn-wallet__value fn-nums">{ "…" }</span> },
                            Some(None) => html! { <span class="fn-wallet__value fn-nums">{ "—" }</span> },
                            Some(Some(v)) => html! { <CountUp value={v} decimals={a.decimals} /> },
                        } }
                    </div>
                }
            }) }
            if matches!(net.kind, ChainKind::Evm) {
                <button
                    type="button"
                    class="topcoat-icon-button--quiet"
                    aria-label={t(lang, Key::refresh_balances)}
                    title={t(lang, Key::refresh_balances)}
                    onclick={refresh_click}
                >
                    { super::super::icons::refresh(16) }
                </button>
            }
        </div>
    }
}

#[allow(clippy::too_many_arguments)]
fn send_form(
    lang: Lang,
    assets: &[Asset],
    asset_ix: &UseStateHandle<usize>,
    to: &UseStateHandle<String>,
    amount: &UseStateHandle<String>,
    advanced: &UseStateHandle<bool>,
    gas_price_gwei: &UseStateHandle<String>,
    gas_limit: &UseStateHandle<String>,
    data_hex: &UseStateHandle<String>,
    estimated_price: &UseStateHandle<Option<u128>>,
    error: &UseStateHandle<Option<String>>,
    estimate: Callback<MouseEvent>,
) -> Html {
    let is_native = assets.get(**asset_ix).is_some_and(|a| a.contract.is_none());
    let symbol = assets
        .get(**asset_ix)
        .map(|a| a.symbol.clone())
        .unwrap_or_default();

    let fee = {
        let price = estimated_price.unwrap_or(FALLBACK_GAS_PRICE);
        let limit: u128 = gas_limit.trim().parse().unwrap_or(21_000);
        format_amount(price.saturating_mul(limit), 18)
    };

    let text_input = |handle: &UseStateHandle<String>| {
        let handle = handle.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                handle.set(el.value());
            }
        })
    };

    html! {
        <>
            <div class="fn-field">
                <label class="fn-field__label" for="send-asset">{ t(lang, Key::asset) }</label>
                <select
                    id="send-asset"
                    class="fn-wallet__network"
                    onchange={{
                        let asset_ix = asset_ix.clone();
                        Callback::from(move |e: Event| {
                            if let Some(sel) = e.target_dyn_into::<HtmlSelectElement>() {
                                asset_ix.set(sel.selected_index().max(0) as usize);
                            }
                        })
                    }}
                >
                    { for assets.iter().enumerate().map(|(i, a)| html! {
                        <option value={i.to_string()} selected={i == **asset_ix}>
                            { if a.contract.is_none() {
                                format!("{} (native)", a.symbol)
                            } else {
                                format!("{} (token)", a.symbol)
                            } }
                        </option>
                    }) }
                </select>
            </div>

            <div class="fn-field">
                <label class="fn-field__label" for="send-to">{ t(lang, Key::recipient) }</label>
                <input
                    id="send-to" class="topcoat-text-input fn-nums" type="text"
                    placeholder="0x…" spellcheck="false"
                    value={(**to).clone()} oninput={text_input(to)}
                />
            </div>

            <div class="fn-field">
                <label class="fn-field__label" for="send-amount">
                    { format!("{} ({symbol})", t(lang, Key::amount)) }
                </label>
                <input
                    id="send-amount" class="topcoat-text-input fn-nums" type="text"
                    inputmode="decimal" placeholder="0.0"
                    value={(**amount).clone()} oninput={text_input(amount)}
                />
            </div>

            <details class="fn-wallet__advanced" open={**advanced}>
                <summary onclick={{
                    let advanced = advanced.clone();
                    Callback::from(move |_: MouseEvent| advanced.set(!*advanced))
                }}>
                    { t(lang, Key::advanced_settings) }
                </summary>
                <div class="fn-field">
                    <label class="fn-field__label" for="send-gas-price">{ t(lang, Key::gas_price_gwei) }</label>
                    <input
                        id="send-gas-price" class="topcoat-text-input fn-nums" type="text"
                        placeholder="2500"
                        value={(**gas_price_gwei).clone()} oninput={text_input(gas_price_gwei)}
                    />
                </div>
                <div class="fn-field">
                    <label class="fn-field__label" for="send-gas-limit">{ t(lang, Key::gas_limit) }</label>
                    <input
                        id="send-gas-limit" class="topcoat-text-input fn-nums" type="text"
                        placeholder="21000"
                        value={(**gas_limit).clone()} oninput={text_input(gas_limit)}
                    />
                </div>
                if is_native {
                    <div class="fn-field">
                        <label class="fn-field__label" for="send-data">
                            { t(lang, Key::data_optional) }
                        </label>
                        <input
                            id="send-data" class="topcoat-text-input fn-nums" type="text"
                            spellcheck="false"
                            value={(**data_hex).clone()} oninput={text_input(data_hex)}
                        />
                    </div>
                }
                <div class="fn-row">
                    <button type="button" class="topcoat-button" onclick={estimate}>
                        { t(lang, Key::estimate) }
                    </button>
                    <span class="fn-field__help fn-nums">
                        { format!("≈ {fee} native fee") }
                    </span>
                </div>
            </details>

            if let Some(e) = &**error {
                <p class="fn-field__error" role="alert">{ e }</p>
            }
        </>
    }
}

#[allow(clippy::too_many_arguments)]
fn confirm_view(
    lang: Lang,
    asset: Option<&Asset>,
    to: &str,
    amount: &str,
    gas_price_gwei: &str,
    gas_limit: &str,
    over_one: bool,
    over_ten: bool,
    retyped: &str,
    phase: &UseStateHandle<Phase>,
) -> Html {
    let symbol = asset.map(|a| a.symbol.clone()).unwrap_or_default();
    html! {
        <div class="fn-wallet__confirm">
            if over_ten {
                <div class="fn-banner fn-banner--warn" role="alert">
                    { t(lang, Key::very_large_amount) }
                </div>
            } else if over_one {
                <div class="fn-banner fn-banner--warn" role="alert">
                    { t(lang, Key::large_amount_warning) }
                </div>
            }

            <dl class="fn-wallet__summary">
                <dt>{ t(lang, Key::send) }</dt>
                <dd class="fn-nums">{ format!("{} {symbol}", amount.trim()) }</dd>
                <dt>{ t(lang, Key::send_to) }</dt>
                <dd class="fn-nums">{ to.trim().to_owned() }</dd>
                <dt>{ t(lang, Key::gas) }</dt>
                <dd class="fn-nums">
                    { format!("{} gwei × {}",
                              if gas_price_gwei.trim().is_empty() { "2500" } else { gas_price_gwei.trim() },
                              if gas_limit.trim().is_empty() { t(lang, Key::gas_auto) } else { gas_limit.trim() }) }
                </dd>
            </dl>

            if over_ten {
                <div class="fn-field">
                    <label class="fn-field__label" for="confirm-amount">
                        { t(lang, Key::retype_the_amount) }
                    </label>
                    <input
                        id="confirm-amount" class="topcoat-text-input fn-nums" type="text"
                        data-autofocus="true"
                        value={retyped.to_owned()}
                        oninput={{
                            let phase = phase.clone();
                            Callback::from(move |e: InputEvent| {
                                if let Some(el) = e.target_dyn_into::<HtmlInputElement>() {
                                    phase.set(Phase::Confirm { retyped: el.value() });
                                }
                            })
                        }}
                    />
                </div>
            }
        </div>
    }
}

fn receipt_view(lang: Lang, outcome: &Outcome) -> Html {
    html! {
        <div class="fn-wallet__receipt" data-ok={outcome.ok.to_string()}>
            <div class="fn-wallet__stamp" aria-hidden="true">
                { if outcome.ok { "✓" } else { "✕" } }
            </div>
            <p class="fn-wallet__verdict">{ &outcome.detail }</p>
            if let (Some(hash), Some(url)) = (&outcome.tx_hash, &outcome.explorer) {
                <p class="fn-nums">
                    <a href={url.clone()} target="_blank" rel="noopener noreferrer">
                        { format!("{}…{}", &hash[..10.min(hash.len())],
                                  &hash[hash.len().saturating_sub(8)..]) }
                    </a>
                </p>
            }
            <dl class="fn-wallet__summary">
                if let Some(gas) = outcome.gas_used {
                    <dt>{ t(lang, Key::gas_used) }</dt>
                    <dd class="fn-nums">{ gas.to_string() }</dd>
                }
                if let Some(before) = outcome.balance_before {
                    <dt>{ t(lang, Key::balance_before) }</dt>
                    <dd class="fn-nums">{ format_amount(before, 18) }</dd>
                }
                if let Some(after) = outcome.balance_after {
                    <dt>{ t(lang, Key::balance_after) }</dt>
                    <dd class="fn-nums">{ format_amount(after, 18) }</dd>
                }
            </dl>
        </div>
    }
}
