//! The Bank — a standalone, more advanced blockchain surface, ported from
//! the reference client's bank page (`pages/bank.tsx`). A full screen at
//! `/bank` since 2026-07: six tabs, a portfolio hero and an agent chat had
//! outgrown the modal they started in.
//!
//! Unlike the wallet dialog — which operates strictly on the chain the
//! *server* configures — the Bank is a universal wallet: it picks its own
//! network (Cronos mainnet or testnet, persisted per browser), so mainnet
//! swaps and free testnet experiments coexist. Six tabs: Portfolio, Send,
//! Swap (VVS), Tokens (import/deploy ERC-20), Greeter, and the AI Banker.
//!
//! Calldata comes from `pocketskynet_core::{abi, bank}` — pure, host-tested
//! bytes. This module owns only what a browser must own: RPC round trips,
//! wall-clock deadlines, localStorage persistence, and the forms.
//!
//! The AI Banker executes: it is the reference client's tool-calling Fruit
//! Banker (`services/bankAgent.ts`), ported with its confirmation policy
//! intact — a swap, a mainnet token transaction or a native send above one
//! unit each stop at an explicit approval dialog before anything is signed
//! (`crate::bank_agent`). An earlier revision of this module declined to
//! reproduce an executing agent; that stance was reversed on request, and
//! the approval dialog is the condition the reversal came with.

use pocketskynet_core::chain::{builtin_networks, format_amount, parse_amount};
use pocketskynet_core::{bank, Network, Token, WalletAddress};
use yew::prelude::*;

use crate::i18n::{t, Key, Lang};
use crate::rpc::EvmRpc;
use crate::state::use_store;

use super::common::Spinner;
use super::toast;

// ---------------------------------------------------------- token registry --

fn tokens_key(chain_id: u64) -> String {
    format!("ps-bank-tokens-{chain_id}")
}

fn greeters_key(chain_id: u64) -> String {
    format!("ps-greeters-{chain_id}")
}

const BANK_NET_KEY: &str = "ps-bank-net";

/// Custom (imported or deployed) tokens for a chain, from localStorage.
pub fn custom_tokens(chain_id: u64) -> Vec<Token> {
    use gloo_storage::Storage;
    gloo_storage::LocalStorage::get(tokens_key(chain_id)).unwrap_or_default()
}

pub(crate) fn save_custom_tokens(chain_id: u64, tokens: &[Token]) {
    use gloo_storage::Storage;
    let _ = gloo_storage::LocalStorage::set(tokens_key(chain_id), tokens);
}

pub(crate) fn saved_greeters(chain_id: u64) -> Vec<String> {
    use gloo_storage::Storage;
    gloo_storage::LocalStorage::get(greeters_key(chain_id)).unwrap_or_default()
}

pub(crate) fn save_greeters(chain_id: u64, list: &[String]) {
    use gloo_storage::Storage;
    let _ = gloo_storage::LocalStorage::set(greeters_key(chain_id), list);
}

/// The networks the Bank can point at: the EVM entries of the built-in
/// registry (Cronos mainnet + testnet). The server's choice is irrelevant
/// here by design.
fn bank_networks() -> Vec<Network> {
    builtin_networks()
        .into_iter()
        .filter(|n| n.chain_id.is_some() && n.supports_send())
        .collect()
}

fn load_bank_chain() -> u64 {
    use gloo_storage::Storage;
    gloo_storage::LocalStorage::get(BANK_NET_KEY).unwrap_or(bank::VVS_CHAIN_ID)
}

fn save_bank_chain(chain_id: u64) {
    use gloo_storage::Storage;
    let _ = gloo_storage::LocalStorage::set(BANK_NET_KEY, chain_id);
}

/// Every token the Bank knows for a network: the server's registry entries
/// when they match this chain, the built-in mainnet set, and locally
/// imported/deployed ones — deduplicated by contract.
pub fn extra_tokens(net: &Network) -> Vec<Token> {
    let Some(chain_id) = net.chain_id else {
        return Vec::new();
    };
    let mut seen: Vec<String> = net
        .tokens
        .iter()
        .map(|t| t.contract.to_lowercase())
        .collect();
    let mut out = Vec::new();
    for token in bank::known_tokens(chain_id)
        .into_iter()
        .chain(custom_tokens(chain_id))
    {
        let key = token.contract.to_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            out.push(token);
        }
    }
    out
}

// ----------------------------------------------------------- tx machinery --

pub(crate) struct TxDone {
    pub(crate) tx_hash: String,
    pub(crate) contract_address: Option<String>,
}

/// Pay the operator's FruitNation wallet `price_cro` on the server's chain,
/// returning the transaction hash — the ticket the paid features (Shout, web
/// publishing) present to their endpoints. Shares [`send_contract_tx`]'s
/// machinery, HUD included, so a paid feature looks and behaves like every
/// other transaction the app signs.
pub(crate) async fn pay_operator(
    store: &crate::state::Store,
    price_cro: &str,
) -> Result<String, String> {
    let lang = store.language;
    let net = store
        .active_network()
        .cloned()
        .ok_or_else(|| t(lang, Key::shout_no_network).to_owned())?;
    let operator = store.chain.fruitnation_wallet.trim().to_owned();
    let to = WalletAddress::new(&operator)
        .map_err(|_| t(lang, Key::shout_no_operator_wallet).to_owned())?;
    let me = store
        .me()
        .cloned()
        .ok_or_else(|| t(lang, Key::shout_no_network).to_owned())?;
    let keys = store
        .auth
        .session()
        .map(|s| s.keys.clone())
        .ok_or_else(|| t(lang, Key::shout_no_network).to_owned())?;
    let value = pocketskynet_core::chain::parse_amount(price_cro, 18)
        .map_err(|_| t(lang, Key::shout_no_operator_wallet).to_owned())?;

    let done = send_contract_tx(lang, &net, &me, keys, Some(to), value, Vec::new(), 30_000).await?;
    Ok(done.tx_hash)
}

/// Sign, broadcast, and wait out one contract transaction. `to: None` deploys.
///
/// Every value-moving path in the Bank and the AI Banker funnels through
/// here, which is why the Skynet relay HUD (burst.rs `tx_*`) is raised here
/// and nowhere else: one hook, full coverage.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_contract_tx(
    lang: Lang,
    net: &Network,
    me: &WalletAddress,
    keys: std::rc::Rc<std::cell::RefCell<crate::crypto::SessionKeys>>,
    to: Option<WalletAddress>,
    value: u128,
    data: Vec<u8>,
    gas_fallback: u128,
) -> Result<TxDone, String> {
    send_contract_tx_with(lang, net, me, keys, to, value, data, gas_fallback, None).await
}

/// [`send_contract_tx`], with a phase tap for callers that narrate progress
/// in their own surface (the AI Banker's chat bubble) on top of the HUD.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_contract_tx_with(
    lang: Lang,
    net: &Network,
    me: &WalletAddress,
    keys: std::rc::Rc<std::cell::RefCell<crate::crypto::SessionKeys>>,
    to: Option<WalletAddress>,
    value: u128,
    data: Vec<u8>,
    gas_fallback: u128,
    on_phase: Option<Callback<super::burst::TxPhase>>,
) -> Result<TxDone, String> {
    let hud = super::burst::tx_start();
    let out = send_contract_tx_inner(
        lang,
        net,
        me,
        keys,
        to,
        value,
        data,
        gas_fallback,
        hud,
        on_phase,
    )
    .await;
    super::burst::tx_end(hud, out.is_ok());
    out
}

#[allow(clippy::too_many_arguments)]
async fn send_contract_tx_inner(
    lang: Lang,
    net: &Network,
    me: &WalletAddress,
    keys: std::rc::Rc<std::cell::RefCell<crate::crypto::SessionKeys>>,
    to: Option<WalletAddress>,
    value: u128,
    data: Vec<u8>,
    gas_fallback: u128,
    hud: u64,
    on_phase: Option<Callback<super::burst::TxPhase>>,
) -> Result<TxDone, String> {
    use super::burst::TxPhase;

    let tx_phase = |hud: u64, phase: TxPhase| {
        super::burst::tx_phase(hud, phase);
        if let Some(cb) = &on_phase {
            cb.emit(phase);
        }
    };

    let rpc = EvmRpc::new(&net.rpc_url);
    let chain_id = net.chain_id.ok_or("no EVM chain id")?;
    tx_phase(hud, TxPhase::Uplink);
    let nonce = rpc.nonce(me).await.map_err(|e| e.to_string())?;
    let gas_price = rpc
        .gas_price()
        .await
        .ok()
        .filter(|p| *p > 0)
        .unwrap_or(2_500_000_000_000);
    let to_hex = to
        .as_ref()
        .map(|a| a.as_str().to_owned())
        .unwrap_or_default();
    let gas_limit = rpc
        .estimate_gas(me, &to_hex, value, &format!("0x{}", hex::encode(&data)))
        .await
        .map(|g| g + g / 5)
        .unwrap_or(gas_fallback);

    let tx = pocketskynet_core::LegacyTransaction {
        nonce,
        gas_price,
        gas_limit,
        to,
        value,
        data,
        chain_id,
    };
    // A browser-wallet session holds no key here, so this would fail with a
    // bare "no signing key on this device". Say what can be done about it
    // instead — and say it before anything is broadcast.
    if !keys.borrow().can_sign_locally() {
        return Err(t(lang, Key::wallet_no_local_key).to_owned());
    }
    tx_phase(hud, TxPhase::Sign);
    let signed = keys
        .borrow()
        .sign_transaction(&tx)
        .map_err(|e| e.to_string())?;
    tx_phase(hud, TxPhase::Broadcast);
    let tx_hash = rpc
        .send_raw_transaction(&signed.raw_hex())
        .await
        .map_err(|e| e.to_string())?;

    tx_phase(hud, TxPhase::Confirm);
    for _ in 0..20 {
        gloo_timers::future::TimeoutFuture::new(3_000).await;
        if let Ok(Some(r)) = rpc.receipt(&tx_hash).await {
            if !r.ok {
                return Err(format!("reverted ({tx_hash})"));
            }
            return Ok(TxDone {
                tx_hash,
                contract_address: r.contract_address,
            });
        }
    }
    // Broadcast accepted but unconfirmed inside our window: report the hash
    // rather than pretending failure.
    Ok(TxDone {
        tx_hash,
        contract_address: None,
    })
}

pub(crate) fn deadline_in_20_minutes() -> u64 {
    (js_sys::Date::now() / 1_000.0) as u64 + 20 * 60
}

// -------------------------------------------------------------------- page --

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BankTab {
    Portfolio,
    Send,
    Swap,
    Tokens,
    Greeter,
    Banker,
}

const BANK_TAB_KEY: &str = "ps-bank-tab";

impl BankTab {
    const ALL: [BankTab; 6] = [
        BankTab::Portfolio,
        BankTab::Send,
        BankTab::Swap,
        BankTab::Tokens,
        BankTab::Greeter,
        BankTab::Banker,
    ];

    fn id(self) -> &'static str {
        match self {
            BankTab::Portfolio => "portfolio",
            BankTab::Send => "send",
            BankTab::Swap => "swap",
            BankTab::Tokens => "tokens",
            BankTab::Greeter => "greeter",
            BankTab::Banker => "banker",
        }
    }

    fn label(self) -> Key {
        match self {
            BankTab::Portfolio => Key::bank_portfolio,
            BankTab::Send => Key::send,
            BankTab::Swap => Key::bank_swap,
            BankTab::Tokens => Key::bank_tokens,
            BankTab::Greeter => Key::bank_greeter,
            BankTab::Banker => Key::ai_banker,
        }
    }

    fn icon(self, size: u16) -> Html {
        match self {
            BankTab::Portfolio => super::icons::wallet(size),
            BankTab::Send => super::icons::send(size),
            BankTab::Swap => super::icons::swap(size),
            BankTab::Tokens => super::icons::coins(size),
            BankTab::Greeter => super::icons::quote(size),
            BankTab::Banker => super::icons::robot(size),
        }
    }

    /// The tab the last visit left open. A page you navigate *to* should come
    /// back the way you left it; a dialog could afford to forget.
    fn load() -> Self {
        use gloo_storage::Storage;
        let id: String = gloo_storage::LocalStorage::get(BANK_TAB_KEY).unwrap_or_default();
        Self::ALL
            .into_iter()
            .find(|t| t.id() == id)
            .unwrap_or(BankTab::Portfolio)
    }

    fn save(self) {
        use gloo_storage::Storage;
        let _ = gloo_storage::LocalStorage::set(BANK_TAB_KEY, self.id());
    }
}

#[function_component(Bank)]
pub fn bank_page() -> Html {
    let store = use_store();
    let lang = store.language;
    let tab = use_state(BankTab::load);
    let chain = use_state(load_bank_chain);
    // Bumped whenever the token registry changes so Portfolio/Send recompute.
    let revision = use_state(|| 0u32);
    // A token symbol staged from Portfolio: tapping a row on the Portfolio
    // tab lands on Send with that asset already picked (the reference's
    // `stagedSend`).
    let staged_send = use_state(|| Option::<String>::None);

    let networks = bank_networks();
    let net = networks
        .iter()
        .find(|n| n.chain_id == Some(*chain))
        .or_else(|| networks.first())
        .cloned();
    let (Some(net), Some(me)) = (net, store.auth.address().cloned()) else {
        return html! {};
    };

    let pick_net = |target: u64, chain: &UseStateHandle<u64>| {
        let chain = chain.clone();
        Callback::from(move |_: MouseEvent| {
            save_bank_chain(target);
            chain.set(target);
        })
    };

    let goto = {
        let tab = tab.clone();
        Callback::from(move |next: BankTab| {
            next.save();
            tab.set(next);
        })
    };

    let on_send_token = {
        let staged_send = staged_send.clone();
        let goto = goto.clone();
        Callback::from(move |symbol: String| {
            staged_send.set(Some(symbol));
            goto.emit(BankTab::Send);
        })
    };

    let tab_button = |this: BankTab, goto: &Callback<BankTab>| {
        let selected = *tab == this;
        let goto = goto.clone();
        html! {
            <button
                type="button"
                class="fn-tab fn-bankpage__tab"
                role="tab"
                aria-selected={selected.to_string()}
                onclick={Callback::from(move |_: MouseEvent| goto.emit(this))}
            >
                { this.icon(17) }
                <span>{ t(lang, this.label()) }</span>
            </button>
        }
    };

    let on_tokens_changed = {
        let revision = revision.clone();
        Callback::from(move |_: ()| revision.set(*revision + 1))
    };
    let _ = *revision;

    html! {
        <div class="fn-bankpage fn-scroll">
            <header class="fn-bankpage__head">
                // The vault emblem (tools/genart.py `bank-emblem`) — Skynet's
                // seal on the door. Tapping it raises the spotlight, because
                // an emblem this good deserves to be seen at full size.
                <button type="button" class="fn-spot__opener fn-bankpage__art-btn"
                    aria-label={t(lang, Key::menu_bank)}
                    onclick={{
                        let title = t(lang, Key::menu_bank).to_owned();
                        let subtitle = net.name.clone();
                        Callback::from(move |_: MouseEvent| {
                            super::spotlight::show(super::spotlight::Spot {
                                image: "/static/img/bank-emblem.png".into(),
                                title: title.clone(),
                                subtitle: Some(subtitle.clone()),
                                copy: None,
                                hue: 190,
                            });
                        })
                    }}
                >
                    <div class="fn-art fn-art--bank-emblem fn-bankpage__art" aria-hidden="true"></div>
                </button>
                <div class="fn-bankpage__title">
                    <h1 class="fn-bankpage__h1">{ t(lang, Key::menu_bank) }</h1>
                    <p class="fn-muted fn-bankpage__hint">{ t(lang, Key::bank_universal_hint) }</p>
                </div>
                <div class="fn-tabs fn-bank__nets" role="radiogroup" aria-label={t(lang, Key::network)}>
                    { for networks.iter().map(|n| {
                        let id = n.chain_id.unwrap_or_default();
                        let selected = Some(*chain) == n.chain_id;
                        html! {
                            <button
                                type="button"
                                class="fn-tab"
                                role="radio"
                                aria-checked={selected.to_string()}
                                onclick={pick_net(id, &chain)}
                            >{ if n.testnet { t(lang, Key::bank_testnet) } else { t(lang, Key::bank_mainnet) } }</button>
                        }
                    }) }
                </div>
            </header>

            <div class="fn-bankpage__body">
                <nav class="fn-tabs fn-bankpage__rail" role="tablist" aria-label={t(lang, Key::menu_bank)}>
                    { for BankTab::ALL.into_iter().map(|t| tab_button(t, &goto)) }
                </nav>

                <section class="fn-bankpage__content fn-bank">
                    { match *tab {
                        BankTab::Portfolio => html! {
                            <PortfolioView net={net.clone()} me={me.clone()}
                                           on_send_token={on_send_token.clone()}
                                           on_goto={goto.clone()} />
                        },
                        BankTab::Send => html! {
                            <BankSendView net={net.clone()} me={me.clone()}
                                          staged={(*staged_send).clone()} />
                        },
                        BankTab::Swap => html! { <SwapView net={net.clone()} me={me.clone()} /> },
                        BankTab::Tokens => html! {
                            <TokensView net={net.clone()} me={me.clone()}
                                        on_tokens_changed={on_tokens_changed.clone()} />
                        },
                        BankTab::Greeter => html! { <GreeterView net={net.clone()} me={me.clone()} /> },
                        BankTab::Banker => html! {
                            <super::banker::BankerView net={net.clone()} me={me.clone()}
                                                       on_mutation={on_tokens_changed.clone()} />
                        },
                    } }
                </section>
            </div>
        </div>
    }
}

// --------------------------------------------------------------- portfolio --

/// A deterministic hue for a token symbol, so USDC is always the same blue
/// and a freshly deployed token still gets a colour of its own. The named
/// entries mirror the reference's `TOKEN_HUES`; everything else hashes.
pub(crate) fn token_hue(symbol: &str) -> u16 {
    match symbol.to_ascii_uppercase().as_str() {
        "USDC" => 210,
        "USDT" => 160,
        "VVS" => 270,
        "WCRO" | "CRO" | "TCRO" => 195,
        "DAI" => 40,
        "WETH" | "ETH" => 230,
        "WBTC" | "BTC" => 25,
        "ATOM" => 265,
        other => {
            let mut h: u32 = 5381;
            for b in other.bytes() {
                h = h.wrapping_mul(33) ^ u32::from(b);
            }
            (h % 360) as u16
        }
    }
}

/// The 1–2 letter monogram on a token badge.
pub(crate) fn token_monogram(symbol: &str) -> String {
    symbol.chars().take(2).collect::<String>().to_uppercase()
}

#[derive(Properties, PartialEq)]
struct PortfolioProps {
    net: Network,
    me: WalletAddress,
    /// Tapping a token row stages it on the Send tab.
    on_send_token: Callback<String>,
    on_goto: Callback<BankTab>,
}

#[function_component(PortfolioView)]
fn portfolio_view(p: &PortfolioProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let balances = use_state(std::collections::HashMap::<String, u128>::new);
    let native = use_state(|| Option::<u128>::None);
    let revision = use_state(|| 0u32);
    let show_receive = use_state(|| false);
    let loading = use_state(|| true);

    let tokens: Vec<Token> = p
        .net
        .tokens
        .iter()
        .cloned()
        .chain(extra_tokens(&p.net))
        .collect();
    {
        let net = p.net.clone();
        let me = p.me.clone();
        let balances = balances.clone();
        let native = native.clone();
        let loading = loading.clone();
        let contracts: Vec<String> = tokens.iter().map(|t| t.contract.clone()).collect();
        use_effect_with((net.id.clone(), contracts.len(), *revision), move |_| {
            let rpc = EvmRpc::new(&net.rpc_url);
            loading.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                native.set(rpc.balance(&me).await.ok());
                let mut map = std::collections::HashMap::new();
                let data = format!(
                    "0x{}",
                    hex::encode(pocketskynet_core::chain::erc20_balance_of_data(&me))
                );
                for contract in contracts {
                    if let Ok(out) = rpc.eth_call(&contract, &data).await {
                        if let Ok(v) = pocketskynet_core::chain::decode_abi_uint(&out) {
                            map.insert(contract.to_lowercase(), v);
                        }
                    }
                }
                balances.set(map);
                loading.set(false);
            });
            || ()
        });
    }

    let copy = {
        let store = store.clone();
        let addr = p.me.to_checksummed();
        Callback::from(move |_: MouseEvent| {
            if super::common::copy_to_clipboard(&addr) {
                toast::success(&store, t(store.language, Key::address_copied));
            }
        })
    };

    let quick = |icon: Html, label: &'static str, on_click: Callback<MouseEvent>| {
        html! {
            <button type="button" class="fn-bank__quick" onclick={on_click}>
                <span class="fn-bank__quick-icon">{ icon }</span>
                <span>{ label }</span>
            </button>
        }
    };

    html! {
        <div class="fn-stack fn-bank__pane">
            // The vault hall rides in this card's background (app.css) —
            // the small teller badge it replaced lives on in the page header.
            <div class="fn-bank__hero">
                <div class="fn-bank__hero-body">
                <div class="fn-muted">{ &p.net.name }</div>
                <div class="fn-bank__hero-amount fn-nums" data-loading={loading.to_string()}>
                    { native.map(|b| format_amount(b, p.net.decimals)).unwrap_or_else(|| "…".into()) }
                    { " " }<span class="fn-bank__hero-symbol">{ &p.net.symbol }</span>
                </div>
                <div class="fn-row">
                    <button type="button" class="topcoat-button" onclick={copy}>
                        { p.me.abbreviated() }
                    </button>
                    <a class="topcoat-button" target="_blank" rel="noopener noreferrer"
                       href={p.net.address_url(p.me.as_str())}>
                        { t(lang, Key::open_explorer) }
                    </a>
                    <button type="button" class="topcoat-icon-button--quiet"
                        aria-label={t(lang, Key::refresh_balances)}
                        data-spinning={loading.to_string()}
                        onclick={{
                            let revision = revision.clone();
                            Callback::from(move |_: MouseEvent| revision.set(*revision + 1))
                        }}>
                        { super::icons::refresh(16) }
                    </button>
                </div>
                </div>
            </div>

            // Quick actions — the four verbs of this screen, one tap each.
            <div class="fn-bank__quickrow" role="group" aria-label={t(lang, Key::bank_quick_actions)}>
                { quick(super::icons::send(18), t(lang, Key::send), {
                    let on_goto = p.on_goto.clone();
                    Callback::from(move |_| on_goto.emit(BankTab::Send))
                }) }
                { quick(super::icons::swap(18), t(lang, Key::bank_swap), {
                    let on_goto = p.on_goto.clone();
                    Callback::from(move |_| on_goto.emit(BankTab::Swap))
                }) }
                { quick(super::icons::download(18), t(lang, Key::bank_receive), {
                    let show_receive = show_receive.clone();
                    Callback::from(move |_| show_receive.set(!*show_receive))
                }) }
                { quick(super::icons::robot(18), t(lang, Key::ai_banker), {
                    let on_goto = p.on_goto.clone();
                    Callback::from(move |_| on_goto.emit(BankTab::Banker))
                }) }
            </div>

            if *show_receive {
                <div class="fn-bank__receive" role="note">
                    <div class="fn-muted">{ t(lang, Key::bank_receive_hint) }</div>
                    <code class="fn-nums fn-bank__receive-addr">{ p.me.to_checksummed() }</code>
                    <button type="button" class="topcoat-button" onclick={{
                        let store = store.clone();
                        let addr = p.me.to_checksummed();
                        Callback::from(move |_: MouseEvent| {
                            if super::common::copy_to_clipboard(&addr) {
                                toast::success(&store, t(store.language, Key::address_copied));
                            }
                        })
                    }}>
                        { t(lang, Key::copy_address) }
                    </button>
                </div>
            }

            <ul class="fn-bank__list">
                { for tokens.iter().map(|token| {
                    let balance = balances.get(&token.contract.to_lowercase()).copied();
                    let on_send_token = p.on_send_token.clone();
                    let symbol = token.symbol.clone();
                    html! {
                        <li key={token.contract.clone()}>
                            <button type="button" class="fn-bank__row fn-bank__row--press"
                                title={t(lang, Key::bank_tap_to_send).replace("{symbol}", &token.symbol)}
                                onclick={Callback::from(move |_: MouseEvent|
                                    on_send_token.emit(symbol.clone()))}>
                                <span class="fn-bank__badge"
                                      style={format!("--tok-h:{}", token_hue(&token.symbol))}>
                                    { token_monogram(&token.symbol) }
                                </span>
                                <span class="fn-grow fn-bank__row-name">
                                    <strong>{ &token.symbol }</strong>
                                    <span class="fn-muted">{ &token.name }</span>
                                </span>
                                <span class="fn-nums">
                                    { balance.map(|b| format_amount(b, token.decimals)).unwrap_or_else(|| "…".into()) }
                                </span>
                            </button>
                        </li>
                    }
                }) }
            </ul>
            <p class="fn-muted fn-bank__footnote">{ t(lang, Key::bank_footnote) }</p>
        </div>
    }
}

// -------------------------------------------------------------------- send --

#[derive(Properties, PartialEq)]
struct SendProps {
    net: Network,
    me: WalletAddress,
    /// A symbol staged from Portfolio; consumed once on arrival.
    staged: Option<String>,
}

#[function_component(BankSendView)]
fn bank_send_view(p: &SendProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let assets = swap_assets(&p.net);
    let asset_ix = use_state(|| 0usize);
    let to = use_state(String::new);
    let amount = use_state(String::new);
    let armed = use_state(|| false);
    let busy = use_state(|| false);
    let status = use_state(String::new);

    // Landing from a Portfolio row: pick that asset. Resolved by symbol, and
    // silently ignored when it no longer resolves (the registry may have
    // changed under the staging) — the reference rejects the whole staging
    // with a toast, but here the picker is visible right above the form.
    {
        let asset_ix = asset_ix.clone();
        let symbols: Vec<String> = assets.iter().map(|a| a.symbol.clone()).collect();
        use_effect_with(p.staged.clone(), move |staged| {
            if let Some(symbol) = staged {
                if let Some(ix) = symbols.iter().position(|s| s.eq_ignore_ascii_case(symbol)) {
                    asset_ix.set(ix);
                }
            }
            || ()
        });
    }

    let Some(asset) = assets.get(*asset_ix).cloned() else {
        return html! {};
    };

    let run = {
        let net = p.net.clone();
        let me = p.me.clone();
        let asset = asset.clone();
        let to_str = to.clone();
        let amount = amount.clone();
        let armed = armed.clone();
        let busy = busy.clone();
        let status = status.clone();
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            if *busy {
                return;
            }
            let Ok(recipient) = WalletAddress::new(to_str.trim()) else {
                return;
            };
            let Ok(units) = parse_amount(amount.trim(), asset.decimals) else {
                return;
            };
            if units == 0 {
                return;
            }
            // Two-click ceremony: the first click arms, the second sends.
            if !*armed {
                armed.set(true);
                return;
            }
            let Some(session) = store.auth.session() else {
                toast::error(&store, t(store.language, Key::wallet_locked), None);
                return;
            };
            let keys = session.keys.clone();
            armed.set(false);
            busy.set(true);
            status.set(t(store.language, Key::broadcasting_tx).into());
            let (tx_to, value, data, fallback) = match &asset.contract {
                None => (recipient.clone(), units, Vec::new(), 30_000),
                Some(contract) => (
                    WalletAddress::new(contract).unwrap(),
                    0,
                    pocketskynet_core::chain::erc20_transfer_data(&recipient, units),
                    100_000,
                ),
            };
            let net = net.clone();
            let me = me.clone();
            let amount = amount.clone();
            let busy = busy.clone();
            let status = status.clone();
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome =
                    send_contract_tx(lang, &net, &me, keys, Some(tx_to), value, data, fallback)
                        .await;
                busy.set(false);
                match outcome {
                    Ok(done) => {
                        status.set(format!(
                            "{} · {}",
                            t(store.language, Key::transaction_confirmed),
                            done.tx_hash
                        ));
                        amount.set(String::new());
                        toast::success(&store, t(store.language, Key::transaction_confirmed));
                    }
                    Err(e) => {
                        status.set(String::new());
                        toast::error(&store, t(store.language, Key::tx_failed_generic), Some(e));
                    }
                }
            });
        })
    };

    html! {
        <div class="fn-stack fn-bank__pane">
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::asset) }</span>
                <select class="topcoat-select" onchange={{
                    let asset_ix = asset_ix.clone();
                    let armed = armed.clone();
                    Callback::from(move |e: Event| {
                        if let Some(el) = e.target_dyn_into::<web_sys::HtmlSelectElement>() {
                            asset_ix.set(el.value().parse().unwrap_or(0));
                            armed.set(false);
                        }
                    })
                }}>
                    { for assets.iter().enumerate().map(|(i, a)| html! {
                        <option value={i.to_string()} selected={i == *asset_ix}>{ &a.symbol }</option>
                    }) }
                </select>
            </div>
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::recipient) }</span>
                <input class="topcoat-text-input fn-nums" value={(*to).clone()}
                    aria-label={t(lang, Key::recipient)}
                    oninput={{
                        let to = to.clone();
                        let armed = armed.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                to.set(el.value());
                                armed.set(false);
                            }
                        })
                    }} />
            </div>
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::amount) }</span>
                <input class="topcoat-text-input fn-nums" value={(*amount).clone()}
                    aria-label={t(lang, Key::amount)}
                    oninput={{
                        let amount = amount.clone();
                        let armed = armed.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                amount.set(el.value());
                                armed.set(false);
                            }
                        })
                    }} />
            </div>
            if !status.is_empty() {
                <p class="fn-muted fn-nums">{ &*status }</p>
            }
            <button type="button"
                class={if *armed { "topcoat-button--cta-danger topcoat-button--cta" } else { "topcoat-button--cta" }}
                disabled={*busy}
                onclick={run}>
                if *busy {
                    <Spinner />
                } else if *armed {
                    { t(lang, Key::send_amount)
                        .replace("{amount}", amount.trim())
                        .replace("{symbol}", &asset.symbol) }
                } else {
                    { t(lang, Key::review_send) }
                }
            </button>
        </div>
    }
}

// -------------------------------------------------------------------- swap --

#[derive(Properties, PartialEq)]
struct ViewProps {
    net: Network,
    me: WalletAddress,
}

#[derive(Clone, PartialEq)]
struct SwapAsset {
    symbol: String,
    decimals: u8,
    /// `None` = native CRO.
    contract: Option<String>,
}

fn swap_assets(net: &Network) -> Vec<SwapAsset> {
    let mut out = vec![SwapAsset {
        symbol: net.symbol.clone(),
        decimals: net.decimals,
        contract: None,
    }];
    for token in net.tokens.iter().cloned().chain(extra_tokens(net)) {
        out.push(SwapAsset {
            symbol: token.symbol,
            decimals: token.decimals,
            contract: Some(token.contract),
        });
    }
    out
}

#[derive(Clone, PartialEq)]
struct Quote {
    amount_in: u128,
    amount_out: u128,
    path: Vec<WalletAddress>,
    /// Native↔WCRO — a 1:1 deposit/withdraw, no router involved.
    wrap: bool,
    /// The *picked* output asset was native CRO. The path alone cannot say —
    /// a native output and a WCRO-token output both end at the WCRO address,
    /// and only one of them must unwrap.
    native_out: bool,
}

/// The router leg for an asset: its contract, or WCRO for native.
fn leg(asset: &SwapAsset) -> WalletAddress {
    let hex = asset
        .contract
        .as_deref()
        .unwrap_or(bank::WCRO_CRONOS_MAINNET);
    WalletAddress::new(hex).expect("registry addresses are validated by core tests")
}

fn is_wcro(asset: &SwapAsset) -> bool {
    asset
        .contract
        .as_deref()
        .is_some_and(|c| c.eq_ignore_ascii_case(bank::WCRO_CRONOS_MAINNET))
}

#[function_component(SwapView)]
fn swap_view(p: &ViewProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let assets = swap_assets(&p.net);
    let from_ix = use_state(|| 0usize);
    let to_ix = use_state(|| 1usize.min(assets.len().saturating_sub(1)));
    let amount = use_state(String::new);
    let slippage = use_state(|| "0.5".to_owned());
    let quote = use_state(|| Option::<Quote>::None);
    let busy = use_state(|| false);
    let status = use_state(String::new);

    if p.net.chain_id != Some(bank::VVS_CHAIN_ID) {
        return html! {
            <div class="fn-banner fn-banner--warn" role="note">
                { t(lang, Key::swap_mainnet_only) }
            </div>
        };
    }
    let (Some(from), Some(to)) = (assets.get(*from_ix).cloned(), assets.get(*to_ix).cloned())
    else {
        return html! {};
    };
    let wrap_mode =
        (from.contract.is_none() && is_wcro(&to)) || (to.contract.is_none() && is_wcro(&from));

    let select = |ix: &UseStateHandle<usize>, quote: &UseStateHandle<Option<Quote>>| {
        let ix = ix.clone();
        let quote = quote.clone();
        Callback::from(move |e: Event| {
            if let Some(el) = e.target_dyn_into::<web_sys::HtmlSelectElement>() {
                ix.set(el.value().parse().unwrap_or(0));
                quote.set(None);
            }
        })
    };

    let get_quote = {
        let net = p.net.clone();
        let from = from.clone();
        let to = to.clone();
        let amount = amount.clone();
        let quote = quote.clone();
        let busy = busy.clone();
        let status = status.clone();
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            let Ok(amount_in) = parse_amount(amount.trim(), from.decimals) else {
                return;
            };
            if amount_in == 0 || *busy {
                return;
            }
            if (from.contract.is_none() && is_wcro(&to))
                || (to.contract.is_none() && is_wcro(&from))
            {
                quote.set(Some(Quote {
                    amount_in,
                    amount_out: amount_in,
                    path: Vec::new(),
                    wrap: true,
                    native_out: to.contract.is_none(),
                }));
                return;
            }
            busy.set(true);
            status.set(String::new());
            let rpc = EvmRpc::new(&net.rpc_url);
            let (from, to) = (from.clone(), to.clone());
            let quote = quote.clone();
            let busy = busy.clone();
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let router = bank::VVS_ROUTER_CRONOS_MAINNET;
                let wcro = WalletAddress::new(bank::WCRO_CRONOS_MAINNET).unwrap();
                let direct = vec![leg(&from), leg(&to)];
                // Try the direct pair first; fall back to routing through
                // WCRO — the reference asks the factory, but a failed quote
                // answers the same question with one fewer contract.
                let mut chosen: Option<(Vec<WalletAddress>, u128)> = None;
                for path in [direct.clone(), vec![leg(&from), wcro.clone(), leg(&to)]] {
                    if path.len() == 3 && (path[0] == wcro || path[2] == wcro) {
                        continue; // degenerate: WCRO already an endpoint
                    }
                    let data = format!(
                        "0x{}",
                        hex::encode(bank::get_amounts_out_data(amount_in, &path))
                    );
                    if let Ok(out_hex) = rpc.eth_call(router, &data).await {
                        if let Ok(amounts) = pocketskynet_core::abi::decode_uint_array(&out_hex) {
                            if let Some(&last) = amounts.last() {
                                chosen = Some((path, last));
                                break;
                            }
                        }
                    }
                }
                busy.set(false);
                match chosen {
                    Some((path, amount_out)) => quote.set(Some(Quote {
                        amount_in,
                        amount_out,
                        path,
                        wrap: false,
                        native_out: to.contract.is_none(),
                    })),
                    None => toast::error(&store, t(store.language, Key::quote_failed), None),
                }
            });
        })
    };

    let run_swap = {
        let net = p.net.clone();
        let me = p.me.clone();
        let from = from.clone();
        let quote_state = quote.clone();
        let slippage = slippage.clone();
        let busy = busy.clone();
        let status = status.clone();
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            let Some(q) = (*quote_state).clone() else {
                return;
            };
            if *busy {
                return;
            }
            let Some(session) = store.auth.session() else {
                toast::error(
                    &store,
                    t(store.language, Key::wallet_locked),
                    Some(t(store.language, Key::unlock_to_sign).into()),
                );
                return;
            };
            let keys = session.keys.clone();
            busy.set(true);
            let bps = bank::slippage_bps(&slippage);
            let net = net.clone();
            let me = me.clone();
            let from = from.clone();
            let busy = busy.clone();
            let status = status.clone();
            let quote_state = quote_state.clone();
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let lang = store.language;
                let router = WalletAddress::new(bank::VVS_ROUTER_CRONOS_MAINNET).unwrap();
                let outcome: Result<TxDone, String> = async {
                    if q.wrap {
                        // deposit() with value, or withdraw(amount).
                        let wcro = WalletAddress::new(bank::WCRO_CRONOS_MAINNET).unwrap();
                        let (value, data) = if from.contract.is_none() {
                            (
                                q.amount_in,
                                pocketskynet_core::abi::encode_call("deposit()", &[]),
                            )
                        } else {
                            (
                                0,
                                pocketskynet_core::abi::encode_call(
                                    "withdraw(uint256)",
                                    &[pocketskynet_core::abi::Arg::Uint(q.amount_in)],
                                ),
                            )
                        };
                        status.set(t(lang, Key::broadcasting_tx).into());
                        return send_contract_tx(
                            lang,
                            &net,
                            &me,
                            keys,
                            Some(wcro),
                            value,
                            data,
                            100_000,
                        )
                        .await;
                    }

                    let min_out = bank::apply_slippage_bps(q.amount_out, bps);
                    let deadline = deadline_in_20_minutes();
                    if let Some(contract) = &from.contract {
                        // Router allowance first.
                        let token =
                            WalletAddress::new(contract).map_err(|_| "bad token address")?;
                        let rpc = EvmRpc::new(&net.rpc_url);
                        let data = format!(
                            "0x{}",
                            hex::encode(bank::erc20_allowance_data(&me, &router))
                        );
                        let allowance = rpc
                            .eth_call(contract, &data)
                            .await
                            .ok()
                            .and_then(|o| pocketskynet_core::abi::decode_uint(&o, 0).ok())
                            .unwrap_or(0);
                        if allowance < q.amount_in {
                            status.set(t(lang, Key::approving_token).into());
                            send_contract_tx(
                                lang,
                                &net,
                                &me,
                                keys.clone(),
                                Some(token),
                                0,
                                bank::erc20_approve_data(&router, q.amount_in),
                                bank::GAS_FALLBACK_APPROVE,
                            )
                            .await?;
                        }
                    }
                    status.set(t(lang, Key::waiting_confirmation).into());
                    let (value, data) = if from.contract.is_none() {
                        (
                            q.amount_in,
                            bank::swap_exact_eth_for_tokens_data(min_out, &q.path, &me, deadline),
                        )
                    } else if q.native_out {
                        (
                            0,
                            bank::swap_exact_tokens_for_eth_data(
                                q.amount_in,
                                min_out,
                                &q.path,
                                &me,
                                deadline,
                            ),
                        )
                    } else {
                        (
                            0,
                            bank::swap_exact_tokens_for_tokens_data(
                                q.amount_in,
                                min_out,
                                &q.path,
                                &me,
                                deadline,
                            ),
                        )
                    };
                    send_contract_tx(
                        lang,
                        &net,
                        &me,
                        keys,
                        Some(router),
                        value,
                        data,
                        bank::GAS_FALLBACK_SWAP,
                    )
                    .await
                }
                .await;
                busy.set(false);
                match outcome {
                    Ok(done) => {
                        status.set(format!(
                            "{} · {}",
                            t(lang, Key::swap_confirmed),
                            done.tx_hash
                        ));
                        toast::success(&store, t(lang, Key::swap_confirmed));
                        quote_state.set(None);
                    }
                    Err(e) => {
                        status.set(String::new());
                        toast::error(&store, t(lang, Key::tx_failed_generic), Some(e));
                    }
                }
            });
        })
    };

    let bps = bank::slippage_bps(&slippage);
    html! {
        <div class="fn-stack fn-bank__pane">
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::from_label) }</span>
                <div class="fn-row">
                    <select class="topcoat-select" onchange={select(&from_ix, &quote)}>
                        { for assets.iter().enumerate().map(|(i, a)| html! {
                            <option value={i.to_string()} selected={i == *from_ix}>{ &a.symbol }</option>
                        }) }
                    </select>
                    <input
                        class="topcoat-text-input fn-grow fn-nums"
                        placeholder={t(lang, Key::amount)}
                        aria-label={t(lang, Key::amount)}
                        value={(*amount).clone()}
                        oninput={{
                            let amount = amount.clone();
                            let quote = quote.clone();
                            Callback::from(move |e: InputEvent| {
                                if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                    amount.set(el.value());
                                    quote.set(None);
                                }
                            })
                        }}
                    />
                </div>
            </div>
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::send_to) }</span>
                <select class="topcoat-select" onchange={select(&to_ix, &quote)}>
                    { for assets.iter().enumerate().map(|(i, a)| html! {
                        <option value={i.to_string()} selected={i == *to_ix}>{ &a.symbol }</option>
                    }) }
                </select>
            </div>
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, Key::slippage_pct) }</span>
                <input
                    class="topcoat-text-input fn-nums"
                    value={(*slippage).clone()}
                    aria-label={t(lang, Key::slippage_pct)}
                    oninput={{
                        let slippage = slippage.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                slippage.set(el.value());
                            }
                        })
                    }}
                />
            </div>

            if wrap_mode {
                <p class="fn-muted">{ t(lang, Key::wrap_one_to_one) }</p>
            }
            if let Some(q) = &*quote {
                <p class="fn-bank__quote fn-nums">
                    { t(lang, Key::quote_line)
                        .replace("{out}", &format_amount(q.amount_out, to.decimals))
                        .replace("{sym}", &to.symbol)
                        .replace("{min}", &format_amount(
                            if q.wrap { q.amount_out } else { bank::apply_slippage_bps(q.amount_out, bps) },
                            to.decimals))
                        .replace("{slip}", &format!("{}", bps as f64 / 100.0)) }
                </p>
            }
            if !status.is_empty() {
                <p class="fn-muted">{ &*status }</p>
            }

            <div class="fn-row">
                <button type="button" class="topcoat-button" disabled={*busy || *from_ix == *to_ix}
                        onclick={get_quote}>
                    { t(lang, Key::get_quote) }
                </button>
                <button type="button" class="topcoat-button--cta"
                        disabled={*busy || quote.is_none()}
                        onclick={run_swap}>
                    if *busy { <Spinner /> } else { { t(lang, Key::swap_now) } }
                </button>
            </div>
        </div>
    }
}

#[function_component(TokensView)]
fn tokens_view(p: &TokensViewProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let chain_id = p.net.chain_id.unwrap_or_default();
    let revision = use_state(|| 0u32);
    let import_addr = use_state(String::new);
    let dep_name = use_state(String::new);
    let dep_symbol = use_state(String::new);
    let dep_decimals = use_state(|| "18".to_owned());
    let dep_supply = use_state(String::new);
    let busy = use_state(|| false);
    let balances = use_state(std::collections::HashMap::<String, u128>::new);

    let all_tokens: Vec<Token> = p
        .net
        .tokens
        .iter()
        .cloned()
        .chain(extra_tokens(&p.net))
        .collect();
    let custom: Vec<String> = custom_tokens(chain_id)
        .iter()
        .map(|t| t.contract.to_lowercase())
        .collect();

    // Balances for the listing, one eth_call per token.
    {
        let net = p.net.clone();
        let me = p.me.clone();
        let balances = balances.clone();
        let contracts: Vec<(String, ())> = all_tokens
            .iter()
            .map(|t| (t.contract.clone(), ()))
            .collect();
        use_effect_with((contracts.len(), *revision), move |_| {
            let rpc = EvmRpc::new(&net.rpc_url);
            wasm_bindgen_futures::spawn_local(async move {
                let mut map = std::collections::HashMap::new();
                for (contract, ()) in contracts {
                    let data = format!(
                        "0x{}",
                        hex::encode(pocketskynet_core::chain::erc20_balance_of_data(&me))
                    );
                    if let Ok(out) = rpc.eth_call(&contract, &data).await {
                        if let Ok(v) = pocketskynet_core::chain::decode_abi_uint(&out) {
                            map.insert(contract.to_lowercase(), v);
                        }
                    }
                }
                balances.set(map);
            });
            || ()
        });
    }

    let import = {
        let net = p.net.clone();
        let import_addr = import_addr.clone();
        let revision = revision.clone();
        let busy = busy.clone();
        let store = store.clone();
        let on_changed = p.on_tokens_changed.clone();
        Callback::from(move |_: MouseEvent| {
            let addr = import_addr.trim().to_lowercase();
            if WalletAddress::new(&addr).is_err() || *busy {
                return;
            }
            busy.set(true);
            let rpc = EvmRpc::new(&net.rpc_url);
            let import_addr = import_addr.clone();
            let revision = revision.clone();
            let busy = busy.clone();
            let store = store.clone();
            let on_changed = on_changed.clone();
            wasm_bindgen_futures::spawn_local(async move {
                use pocketskynet_core::abi;
                let symbol = rpc
                    .eth_call(
                        &addr,
                        &format!("0x{}", hex::encode(bank::erc20_symbol_data())),
                    )
                    .await
                    .ok()
                    .and_then(|o| abi::decode_string(&o).ok());
                let name = rpc
                    .eth_call(
                        &addr,
                        &format!("0x{}", hex::encode(bank::erc20_name_data())),
                    )
                    .await
                    .ok()
                    .and_then(|o| abi::decode_string(&o).ok());
                let decimals = rpc
                    .eth_call(
                        &addr,
                        &format!("0x{}", hex::encode(bank::erc20_decimals_data())),
                    )
                    .await
                    .ok()
                    .and_then(|o| abi::decode_uint(&o, 0).ok());
                busy.set(false);
                let (Some(symbol), Some(decimals)) = (symbol, decimals) else {
                    toast::error(&store, t(store.language, Key::not_an_erc20), None);
                    return;
                };
                let chain_id = store
                    .active_network()
                    .and_then(|n| n.chain_id)
                    .unwrap_or_default();
                let mut list = custom_tokens(chain_id);
                if !list.iter().any(|t| t.contract.eq_ignore_ascii_case(&addr)) {
                    list.push(Token {
                        symbol: symbol.clone(),
                        name: name.unwrap_or(symbol),
                        contract: addr,
                        decimals: decimals.min(36) as u8,
                    });
                    save_custom_tokens(chain_id, &list);
                }
                import_addr.set(String::new());
                revision.set(*revision + 1);
                on_changed.emit(());
                toast::success(&store, t(store.language, Key::token_added));
            });
        })
    };

    let deploy = {
        let net = p.net.clone();
        let me = p.me.clone();
        let dep_name = dep_name.clone();
        let dep_symbol = dep_symbol.clone();
        let dep_decimals = dep_decimals.clone();
        let dep_supply = dep_supply.clone();
        let revision = revision.clone();
        let busy = busy.clone();
        let store = store.clone();
        let on_changed = p.on_tokens_changed.clone();
        Callback::from(move |_: MouseEvent| {
            let name = dep_name.trim().to_owned();
            let symbol = dep_symbol.trim().to_owned();
            let decimals: u8 = dep_decimals.trim().parse().unwrap_or(18).min(18);
            if name.is_empty() || symbol.is_empty() || *busy {
                return;
            }
            let Ok(supply) = parse_amount(dep_supply.trim(), decimals) else {
                return;
            };
            let Some(session) = store.auth.session() else {
                toast::error(&store, t(store.language, Key::wallet_locked), None);
                return;
            };
            let keys = session.keys.clone();
            let Ok(data) = bank::erc20_deploy_data(&name, &symbol, decimals, supply) else {
                return;
            };
            busy.set(true);
            let net = net.clone();
            let me = me.clone();
            let dep_name = dep_name.clone();
            let dep_symbol = dep_symbol.clone();
            let dep_supply = dep_supply.clone();
            let revision = revision.clone();
            let busy = busy.clone();
            let store = store.clone();
            let on_changed = on_changed.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = send_contract_tx(
                    lang,
                    &net,
                    &me,
                    keys,
                    None,
                    0,
                    data,
                    bank::GAS_FALLBACK_ERC20_DEPLOY,
                )
                .await;
                busy.set(false);
                match outcome {
                    Ok(done) => {
                        if let Some(address) = done.contract_address {
                            let chain_id = net.chain_id.unwrap_or_default();
                            let mut list = custom_tokens(chain_id);
                            list.push(Token {
                                symbol: symbol.clone(),
                                name: name.clone(),
                                contract: address.clone(),
                                decimals,
                            });
                            save_custom_tokens(chain_id, &list);
                            toast::success(
                                &store,
                                t(store.language, Key::deployed_at).replace("{address}", &address),
                            );
                        }
                        dep_name.set(String::new());
                        dep_symbol.set(String::new());
                        dep_supply.set(String::new());
                        revision.set(*revision + 1);
                        on_changed.emit(());
                    }
                    Err(e) => {
                        toast::error(&store, t(store.language, Key::tx_failed_generic), Some(e))
                    }
                }
            });
        })
    };

    let remove = {
        let revision = revision.clone();
        let on_changed = p.on_tokens_changed.clone();
        let store = store.clone();
        Callback::from(move |contract: String| {
            let chain_id = store
                .active_network()
                .and_then(|n| n.chain_id)
                .unwrap_or_default();
            let list: Vec<Token> = custom_tokens(chain_id)
                .into_iter()
                .filter(|t| !t.contract.eq_ignore_ascii_case(&contract))
                .collect();
            save_custom_tokens(chain_id, &list);
            revision.set(*revision + 1);
            on_changed.emit(());
        })
    };

    let field = |label: Key, state: &UseStateHandle<String>, numeric: bool| {
        let state = state.clone();
        html! {
            <div class="fn-field">
                <span class="fn-field__label">{ t(lang, label) }</span>
                <input
                    class={classes!("topcoat-text-input", numeric.then_some("fn-nums"))}
                    value={(*state).clone()}
                    aria-label={t(lang, label)}
                    oninput={Callback::from(move |e: InputEvent| {
                        if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                            state.set(el.value());
                        }
                    })}
                />
            </div>
        }
    };

    html! {
        <div class="fn-stack fn-bank__pane">
            <ul class="fn-bank__list">
                { for all_tokens.iter().map(|token| {
                    let is_custom = custom.contains(&token.contract.to_lowercase());
                    let balance = balances.get(&token.contract.to_lowercase()).copied();
                    let remove = remove.clone();
                    let contract = token.contract.clone();
                    html! {
                        <li class="fn-bank__row" key={token.contract.clone()}>
                            <div class="fn-grow">
                                <strong>{ &token.symbol }</strong>
                                { " " }
                                <span class="fn-muted">{ &token.name }</span>
                                <div class="fn-muted fn-nums fn-bank__addr">{ &token.contract }</div>
                            </div>
                            <span class="fn-nums">
                                { balance.map(|b| format_amount(b, token.decimals)).unwrap_or_default() }
                            </span>
                            if is_custom {
                                <button type="button" class="topcoat-button--quiet"
                                    onclick={Callback::from(move |_: MouseEvent| remove.emit(contract.clone()))}>
                                    { t(lang, Key::remove) }
                                </button>
                            }
                        </li>
                    }
                }) }
            </ul>

            <h3 class="topcoat-list__header">{ t(lang, Key::import_token) }</h3>
            <div class="fn-row">
                <input
                    class="topcoat-text-input fn-grow fn-nums"
                    placeholder={t(lang, Key::token_address)}
                    aria-label={t(lang, Key::token_address)}
                    value={(*import_addr).clone()}
                    oninput={{
                        let import_addr = import_addr.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                import_addr.set(el.value());
                            }
                        })
                    }}
                />
                <button type="button" class="topcoat-button" disabled={*busy} onclick={import}>
                    { t(lang, Key::import_token) }
                </button>
            </div>

            <h3 class="topcoat-list__header">{ t(lang, Key::deploy_token) }</h3>
            { field(Key::token_name, &dep_name, false) }
            { field(Key::token_symbol, &dep_symbol, false) }
            { field(Key::token_decimals, &dep_decimals, true) }
            { field(Key::initial_supply, &dep_supply, true) }
            <button type="button" class="topcoat-button--cta" disabled={*busy} onclick={deploy}>
                if *busy { <Spinner /> { " " } { t(lang, Key::deploying) } }
                else { { t(lang, Key::deploy_token) } }
            </button>
        </div>
    }
}

#[derive(Properties, PartialEq)]
struct TokensViewProps {
    net: Network,
    me: WalletAddress,
    on_tokens_changed: Callback<()>,
}

// ----------------------------------------------------------------- greeter --

#[function_component(GreeterView)]
fn greeter_view(p: &ViewProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let chain_id = p.net.chain_id.unwrap_or_default();
    let revision = use_state(|| 0u32);
    let greetings = use_state(std::collections::HashMap::<String, String>::new);
    let deploy_msg = use_state(String::new);
    let attach_addr = use_state(String::new);
    let set_drafts = use_state(std::collections::HashMap::<String, String>::new);
    let busy = use_state(|| false);

    let list = saved_greeters(chain_id);

    // Fetch every saved greeter's current greeting.
    {
        let net = p.net.clone();
        let greetings = greetings.clone();
        let list = list.clone();
        use_effect_with((list.len(), *revision), move |_| {
            let rpc = EvmRpc::new(&net.rpc_url);
            wasm_bindgen_futures::spawn_local(async move {
                let mut map = std::collections::HashMap::new();
                let data = format!("0x{}", hex::encode(bank::greet_data()));
                for address in list {
                    if let Ok(out) = rpc.eth_call(&address, &data).await {
                        if let Ok(text) = pocketskynet_core::abi::decode_string(&out) {
                            map.insert(address, text);
                        }
                    }
                }
                greetings.set(map);
            });
            || ()
        });
    }

    let with_session = |store: &crate::state::Store| -> Option<std::rc::Rc<std::cell::RefCell<crate::crypto::SessionKeys>>> {
        match store.auth.session() {
            Some(s) => Some(s.keys.clone()),
            None => {
                toast::error(store, t(store.language, Key::wallet_locked), None);
                None
            }
        }
    };

    let deploy = {
        let net = p.net.clone();
        let me = p.me.clone();
        let deploy_msg = deploy_msg.clone();
        let revision = revision.clone();
        let busy = busy.clone();
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            let message = deploy_msg.trim().to_owned();
            if message.is_empty() || *busy {
                return;
            }
            let Some(keys) = with_session(&store) else {
                return;
            };
            let Ok(data) = bank::greeter_deploy_data(&message) else {
                return;
            };
            busy.set(true);
            let net = net.clone();
            let me = me.clone();
            let deploy_msg = deploy_msg.clone();
            let revision = revision.clone();
            let busy = busy.clone();
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = send_contract_tx(
                    lang,
                    &net,
                    &me,
                    keys,
                    None,
                    0,
                    data,
                    bank::GAS_FALLBACK_GREETER_DEPLOY,
                )
                .await;
                busy.set(false);
                match outcome {
                    Ok(done) => {
                        if let Some(address) = done.contract_address {
                            let chain_id = net.chain_id.unwrap_or_default();
                            let mut list = saved_greeters(chain_id);
                            list.push(address.clone());
                            save_greeters(chain_id, &list);
                            toast::success(
                                &store,
                                t(store.language, Key::deployed_at).replace("{address}", &address),
                            );
                        }
                        deploy_msg.set(String::new());
                        revision.set(*revision + 1);
                    }
                    Err(e) => {
                        toast::error(&store, t(store.language, Key::tx_failed_generic), Some(e))
                    }
                }
            });
        })
    };

    let attach = {
        let net = p.net.clone();
        let attach_addr = attach_addr.clone();
        let revision = revision.clone();
        let busy = busy.clone();
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            let addr = attach_addr.trim().to_lowercase();
            if WalletAddress::new(&addr).is_err() || *busy {
                return;
            }
            busy.set(true);
            let rpc = EvmRpc::new(&net.rpc_url);
            let net = net.clone();
            let attach_addr = attach_addr.clone();
            let revision = revision.clone();
            let busy = busy.clone();
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let data = format!("0x{}", hex::encode(bank::greet_data()));
                let valid = rpc
                    .eth_call(&addr, &data)
                    .await
                    .ok()
                    .and_then(|o| pocketskynet_core::abi::decode_string(&o).ok())
                    .is_some();
                busy.set(false);
                if !valid {
                    toast::error(&store, t(store.language, Key::not_a_greeter), None);
                    return;
                }
                let chain_id = net.chain_id.unwrap_or_default();
                let mut list = saved_greeters(chain_id);
                if !list.iter().any(|a| a.eq_ignore_ascii_case(&addr)) {
                    list.push(addr);
                    save_greeters(chain_id, &list);
                }
                attach_addr.set(String::new());
                revision.set(*revision + 1);
            });
        })
    };

    let set_greeting = {
        let net = p.net.clone();
        let me = p.me.clone();
        let set_drafts = set_drafts.clone();
        let revision = revision.clone();
        let busy = busy.clone();
        let store = store.clone();
        Callback::from(move |address: String| {
            let message = set_drafts.get(&address).cloned().unwrap_or_default();
            if message.trim().is_empty() || *busy {
                return;
            }
            let Some(keys) = with_session(&store) else {
                return;
            };
            let Ok(to) = WalletAddress::new(&address) else {
                return;
            };
            busy.set(true);
            let data = bank::set_greeting_data(message.trim());
            let net = net.clone();
            let me = me.clone();
            let set_drafts = set_drafts.clone();
            let revision = revision.clone();
            let busy = busy.clone();
            let store = store.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let outcome = send_contract_tx(
                    lang,
                    &net,
                    &me,
                    keys,
                    Some(to),
                    0,
                    data,
                    bank::GAS_FALLBACK_GREETER_SET,
                )
                .await;
                busy.set(false);
                match outcome {
                    Ok(_) => {
                        toast::success(&store, t(store.language, Key::greeting_updated));
                        let mut drafts = (*set_drafts).clone();
                        drafts.remove(&address);
                        set_drafts.set(drafts);
                        revision.set(*revision + 1);
                    }
                    Err(e) => {
                        toast::error(&store, t(store.language, Key::tx_failed_generic), Some(e))
                    }
                }
            });
        })
    };

    let forget = {
        let revision = revision.clone();
        let store = store.clone();
        Callback::from(move |address: String| {
            let chain_id = store
                .active_network()
                .and_then(|n| n.chain_id)
                .unwrap_or_default();
            let list: Vec<String> = saved_greeters(chain_id)
                .into_iter()
                .filter(|a| !a.eq_ignore_ascii_case(&address))
                .collect();
            save_greeters(chain_id, &list);
            revision.set(*revision + 1);
        })
    };

    html! {
        <div class="fn-stack fn-bank__pane">
            if list.is_empty() {
                <p class="fn-muted">{ t(lang, Key::no_greeters_yet) }</p>
            } else {
                <ul class="fn-bank__list">
                    { for list.iter().map(|address| {
                        let greeting = greetings.get(address).cloned();
                        let draft = set_drafts.get(address).cloned().unwrap_or_default();
                        let set_greeting = set_greeting.clone();
                        let forget = forget.clone();
                        let set_drafts = set_drafts.clone();
                        let address_for_input = address.clone();
                        let address_for_set = address.clone();
                        let address_for_forget = address.clone();
                        html! {
                            <li class="fn-bank__row fn-bank__row--stack" key={address.clone()}>
                                <div class="fn-muted fn-nums fn-bank__addr">{ address }</div>
                                <blockquote class="fn-bank__greeting">
                                    { greeting.unwrap_or_else(|| "…".into()) }
                                </blockquote>
                                <div class="fn-row">
                                    <input
                                        class="topcoat-text-input fn-grow"
                                        placeholder={t(lang, Key::new_greeting)}
                                        aria-label={t(lang, Key::new_greeting)}
                                        value={draft}
                                        oninput={Callback::from(move |e: InputEvent| {
                                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                                let mut drafts = (*set_drafts).clone();
                                                drafts.insert(address_for_input.clone(), el.value());
                                                set_drafts.set(drafts);
                                            }
                                        })}
                                    />
                                    <button type="button" class="topcoat-button" disabled={*busy}
                                        onclick={Callback::from(move |_: MouseEvent|
                                            set_greeting.emit(address_for_set.clone()))}>
                                        { t(lang, Key::set_greeting) }
                                    </button>
                                    <button type="button" class="topcoat-button--quiet"
                                        onclick={Callback::from(move |_: MouseEvent|
                                            forget.emit(address_for_forget.clone()))}>
                                        { t(lang, Key::remove) }
                                    </button>
                                </div>
                            </li>
                        }
                    }) }
                </ul>
            }

            <h3 class="topcoat-list__header">{ t(lang, Key::deploy_greeter) }</h3>
            <div class="fn-row">
                <input
                    class="topcoat-text-input fn-grow"
                    placeholder={t(lang, Key::initial_greeting)}
                    aria-label={t(lang, Key::initial_greeting)}
                    value={(*deploy_msg).clone()}
                    oninput={{
                        let deploy_msg = deploy_msg.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                deploy_msg.set(el.value());
                            }
                        })
                    }}
                />
                <button type="button" class="topcoat-button--cta" disabled={*busy} onclick={deploy}>
                    if *busy { <Spinner /> } else { { t(lang, Key::deploy_greeter) } }
                </button>
            </div>

            <h3 class="topcoat-list__header">{ t(lang, Key::attach_existing) }</h3>
            <div class="fn-row">
                <input
                    class="topcoat-text-input fn-grow fn-nums"
                    placeholder={t(lang, Key::greeter_address)}
                    aria-label={t(lang, Key::greeter_address)}
                    value={(*attach_addr).clone()}
                    oninput={{
                        let attach_addr = attach_addr.clone();
                        Callback::from(move |e: InputEvent| {
                            if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                attach_addr.set(el.value());
                            }
                        })
                    }}
                />
                <button type="button" class="topcoat-button" disabled={*busy} onclick={attach}>
                    { t(lang, Key::attach_existing) }
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_token_hues_match_the_reference_palette() {
        // The named entries mirror the reference's TOKEN_HUES: USDC must
        // always be the same blue, whatever order the registry lists it in.
        assert_eq!(token_hue("USDC"), 210);
        assert_eq!(token_hue("usdc"), 210, "case must not change a colour");
        assert_eq!(token_hue("VVS"), 270);
        assert_eq!(token_hue("WBTC"), 25);
        // Native and wrapped native share a hue — they are the same asset to
        // a human scanning the list.
        assert_eq!(token_hue("CRO"), token_hue("WCRO"));
        assert_eq!(token_hue("TCRO"), token_hue("CRO"));
    }

    #[test]
    fn unknown_symbols_hash_to_a_stable_hue_in_range() {
        let a = token_hue("FRUIT");
        assert_eq!(
            a,
            token_hue("FRUIT"),
            "a badge must not change colour between renders"
        );
        assert!(a < 360);
        // Different symbols should not all collapse onto one hue.
        let hues: std::collections::HashSet<u16> = ["FRUIT", "APPLE", "MANGO", "KIWI", "PLUM"]
            .iter()
            .map(|s| token_hue(s))
            .collect();
        assert!(hues.len() >= 3, "the hash should spread, got {hues:?}");
    }

    #[test]
    fn monograms_are_at_most_two_chars_and_uppercase() {
        assert_eq!(token_monogram("usdc"), "US");
        assert_eq!(token_monogram("V"), "V");
        assert_eq!(token_monogram(""), "");
        // char-based, not byte-based: a multibyte symbol must not panic or
        // split a code point.
        assert_eq!(token_monogram("Ξeth"), "ΞE");
    }

    #[test]
    fn bank_tab_ids_round_trip_for_persistence() {
        // `ps-bank-tab` stores `id()` and `load()` finds it in ALL — so every
        // id must be unique, and every tab reachable from its own id.
        let ids: std::collections::HashSet<&str> = BankTab::ALL.iter().map(|t| t.id()).collect();
        assert_eq!(ids.len(), BankTab::ALL.len(), "duplicate tab id");
        for tab in BankTab::ALL {
            let found = BankTab::ALL.into_iter().find(|t| t.id() == tab.id());
            assert_eq!(found, Some(tab));
        }
    }
}
