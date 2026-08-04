//! The AI Banker — the executing half.
//!
//! `crate::bank_agent` owns everything pure (prompt, parser, validators,
//! confirmation policy); this module owns what only a browser can: the RPC
//! round trips behind each tool, the signing path, the approval dialog, the
//! persisted transcript, and the chat surface itself.
//!
//! The loop is the reference's `runBankerTurn`: ask the model; if the whole
//! reply is one `{"tool": …}` object, run the tool and feed the result back
//! as `[TOOL RESULT <name>] …`; prose ends the turn; eight rounds maximum.
//! Value-moving tools stop at an approval dialog per the policy in
//! `bank_agent::needs_confirmation` — declining feeds the model the literal
//! `DECLINED` string, which the prompt tells it to accept gracefully.

use std::cell::RefCell;
use std::rc::Rc;

use pocketskynet_core::chain::{format_amount, parse_amount};
use pocketskynet_core::{abi, bank, Network, Token, WalletAddress};
use serde_json::Value;
use yew::prelude::*;

use crate::ai::{self, ChatTurn};
use crate::bank_agent as agent;
use crate::i18n::{t, Key, Lang};
use crate::rpc::EvmRpc;
use crate::state::use_store;

use super::bank::{
    custom_tokens, extra_tokens, save_custom_tokens, save_greeters, saved_greeters,
    send_contract_tx,
};
use super::common::Spinner;
use super::modal::Modal as Dialog;
use super::toast;

const BANKER_LOG_KEY: &str = "ps-banker-log";

fn load_log() -> Vec<agent::StoredMsg> {
    use gloo_storage::Storage;
    gloo_storage::LocalStorage::get(BANKER_LOG_KEY).unwrap_or_default()
}

fn save_log(log: &[agent::StoredMsg]) {
    use gloo_storage::Storage;
    let _ = gloo_storage::LocalStorage::set(BANKER_LOG_KEY, log);
}

// ------------------------------------------------------------------- stages --

/// What the progress bubble says while a turn runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Thinking,
    Reading(String),
    Sending,
    Confirming,
    Generating,
}

impl Stage {
    fn label(&self, lang: Lang) -> String {
        match self {
            Stage::Thinking => t(lang, Key::banker_thinking).to_owned(),
            Stage::Reading(tool) => format!("{} — {tool}", t(lang, Key::banker_reading)),
            Stage::Sending => t(lang, Key::banker_sending).to_owned(),
            Stage::Confirming => t(lang, Key::banker_confirming).to_owned(),
            Stage::Generating => t(lang, Key::banker_generating).to_owned(),
        }
    }
}

// ----------------------------------------------------------------- approval --

/// A transaction waiting on the user. The decision cell is polled by the
/// paused tool future — a dependency-free promise bridge.
#[derive(Clone, Debug)]
pub struct Approval {
    pub title: String,
    pub lines: Vec<(String, String)>,
    pub decision: Rc<RefCell<Option<bool>>>,
}

impl PartialEq for Approval {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.decision, &other.decision)
    }
}

async fn ask_approval(d: &Deps, title: String, lines: Vec<(String, String)>) -> bool {
    let decision = Rc::new(RefCell::new(None));
    d.approve.emit(Approval {
        title,
        lines,
        decision: decision.clone(),
    });
    loop {
        if let Some(v) = *decision.borrow() {
            return v;
        }
        gloo_timers::future::TimeoutFuture::new(120).await;
    }
}

// --------------------------------------------------------------------- deps --

#[derive(Clone)]
struct Deps {
    lang: Lang,
    provider: ai::Provider,
    key: String,
    image_provider: Option<(ai::Provider, String)>,
    net: Network,
    me: WalletAddress,
    keys: Option<Rc<RefCell<crate::crypto::SessionKeys>>>,
    client: crate::api::Client,
    stage: Callback<Stage>,
    approve: Callback<Approval>,
}

impl Deps {
    fn mainnet(&self) -> bool {
        !self.net.testnet
    }

    fn all_tokens(&self) -> Vec<Token> {
        self.net
            .tokens
            .iter()
            .cloned()
            .chain(extra_tokens(&self.net))
            .collect()
    }

    fn keys(&self) -> Result<Rc<RefCell<crate::crypto::SessionKeys>>, String> {
        self.keys
            .clone()
            .ok_or_else(|| t(self.lang, Key::wallet_locked).to_owned())
    }
}

/// Resolve a listed symbol or an arbitrary 0x address to a token. An unknown
/// address is interrogated on-chain, like the reference's `resolveToken`.
async fn resolve_token(d: &Deps, asset: &str) -> Result<Token, String> {
    let asset = asset.trim();
    if let Some(t) = d
        .all_tokens()
        .into_iter()
        .find(|t| t.symbol.eq_ignore_ascii_case(asset) || t.contract.eq_ignore_ascii_case(asset))
    {
        return Ok(t);
    }
    if WalletAddress::new(asset).is_ok() {
        return fetch_token_meta(d, &asset.to_lowercase()).await;
    }
    Err(format!(
        "unknown token \"{asset}\" — import it first or pass its 0x contract address"
    ))
}

async fn fetch_token_meta(d: &Deps, contract: &str) -> Result<Token, String> {
    let rpc = EvmRpc::new(&d.net.rpc_url);
    let call = |data: Vec<u8>| {
        let rpc = EvmRpc::new(&d.net.rpc_url);
        let contract = contract.to_owned();
        async move {
            rpc.eth_call(&contract, &format!("0x{}", hex::encode(data)))
                .await
                .map_err(|e| e.to_string())
        }
    };
    let symbol = call(bank::erc20_symbol_data())
        .await
        .ok()
        .and_then(|o| abi::decode_string(&o).ok());
    let decimals = rpc
        .eth_call(
            contract,
            &format!("0x{}", hex::encode(bank::erc20_decimals_data())),
        )
        .await
        .ok()
        .and_then(|o| abi::decode_uint(&o, 0).ok());
    let name = call(bank::erc20_name_data())
        .await
        .ok()
        .and_then(|o| abi::decode_string(&o).ok());
    match (symbol, decimals) {
        (Some(symbol), Some(decimals)) => Ok(Token {
            name: name.unwrap_or_else(|| symbol.clone()),
            symbol,
            contract: contract.to_owned(),
            decimals: decimals.min(36) as u8,
        }),
        _ => Err(format!("{contract} does not answer like an ERC-20")),
    }
}

async fn erc20_read(d: &Deps, contract: &str, data: Vec<u8>) -> Result<String, String> {
    EvmRpc::new(&d.net.rpc_url)
        .eth_call(contract, &format!("0x{}", hex::encode(data)))
        .await
        .map_err(|e| e.to_string())
}

fn explorer_tx(net: &Network, hash: &str) -> String {
    net.tx_url(hash)
}

// -------------------------------------------------------------------- tools --

struct ToolFx<'a> {
    images: &'a mut Vec<String>,
    tx_hashes: &'a mut Vec<String>,
}

/// Run one tool. `Ok` strings are fed to the model verbatim; `Err` becomes
/// an `ERROR: …` result (also fed to the model, never thrown at the UI).
async fn exec_tool(
    d: &Deps,
    name: &str,
    args: &Value,
    fx: &mut ToolFx<'_>,
) -> Result<String, String> {
    let rpc = EvmRpc::new(&d.net.rpc_url);
    match name {
        // ------------------------------------------------------------ reads --
        "get_native_balance" => {
            d.stage.emit(Stage::Reading(name.into()));
            let who = match agent::arg_str_opt(args, "address") {
                Some(a) => WalletAddress::new(&a).map_err(|_| format!("bad address {a}"))?,
                None => d.me.clone(),
            };
            let bal = rpc.balance(&who).await.map_err(|e| e.to_string())?;
            Ok(format!(
                "{} {} (address {})",
                format_amount(bal, d.net.decimals),
                d.net.symbol,
                who.to_checksummed()
            ))
        }
        "get_token_balance" => {
            d.stage.emit(Stage::Reading(name.into()));
            let token = resolve_token(d, &agent::arg_str(args, "asset")?).await?;
            let who = match agent::arg_str_opt(args, "address") {
                Some(a) => WalletAddress::new(&a).map_err(|_| format!("bad address {a}"))?,
                None => d.me.clone(),
            };
            let out = erc20_read(
                d,
                &token.contract,
                pocketskynet_core::chain::erc20_balance_of_data(&who),
            )
            .await?;
            let v = abi::decode_uint(&out, 0).map_err(|e| e.to_string())?;
            Ok(format!(
                "{} {} (address {})",
                format_amount(v, token.decimals),
                token.symbol,
                who.to_checksummed()
            ))
        }
        "get_total_supply" => {
            d.stage.emit(Stage::Reading(name.into()));
            let token = resolve_token(d, &agent::arg_str(args, "asset")?).await?;
            let out =
                erc20_read(d, &token.contract, abi::encode_call("totalSupply()", &[])).await?;
            let v = abi::decode_uint(&out, 0).map_err(|e| e.to_string())?;
            Ok(format!(
                "{} {}",
                format_amount(v, token.decimals),
                token.symbol
            ))
        }
        "get_allowance" => {
            d.stage.emit(Stage::Reading(name.into()));
            let token = resolve_token(d, &agent::arg_str(args, "asset")?).await?;
            let owner = agent::arg_address(args, "owner")?;
            let spender = agent::arg_address(args, "spender")?;
            let out = erc20_read(
                d,
                &token.contract,
                bank::erc20_allowance_data(&owner, &spender),
            )
            .await?;
            let v = abi::decode_uint(&out, 0).map_err(|e| e.to_string())?;
            Ok(format!(
                "{} {}",
                format_amount(v, token.decimals),
                token.symbol
            ))
        }
        "get_gas_price" => {
            d.stage.emit(Stage::Reading(name.into()));
            let wei = rpc.gas_price().await.map_err(|e| e.to_string())?;
            Ok(format!("{} gwei", wei / 1_000_000_000))
        }
        "list_tokens" => {
            let lines: Vec<String> = d
                .all_tokens()
                .iter()
                .map(|t| {
                    format!(
                        "{} ({}) at {} — {} decimals",
                        agent::sanitize_onchain_text(&t.symbol, 20),
                        agent::sanitize_onchain_text(&t.name, 48),
                        t.contract,
                        t.decimals
                    )
                })
                .collect();
            Ok(if lines.is_empty() {
                "no tokens known".into()
            } else {
                lines.join("\n")
            })
        }
        "list_greeters" => {
            let list = saved_greeters(d.net.chain_id.unwrap_or_default());
            Ok(if list.is_empty() {
                "no greeter contracts saved".into()
            } else {
                list.join("\n")
            })
        }
        "greeter_get" => {
            d.stage.emit(Stage::Reading(name.into()));
            let address = agent::arg_address(args, "address")?;
            let out = erc20_read(d, address.as_str(), bank::greet_data()).await?;
            let text = abi::decode_string(&out).map_err(|e| e.to_string())?;
            Ok(format!(
                "greeting: \"{}\"",
                agent::sanitize_onchain_text(&text, 280)
            ))
        }
        "swap_quote" => {
            d.stage.emit(Stage::Reading(name.into()));
            let (from, to, amount_in, q) = quote_for_agent(d, args).await?;
            Ok(format!(
                "{} {} ≈ {} {} (path length {})",
                format_amount(amount_in, from.1),
                from.0,
                format_amount(q.amount_out, to.1),
                to.0,
                if q.wrap { 0 } else { q.path.len() }
            ))
        }

        // ----------------------------------------------------- transactions --
        "send_native" => {
            let to = agent::arg_address(args, "to")?;
            let amount = agent::arg_amount(args, "amount")?;
            let units = parse_amount(&amount, d.net.decimals).map_err(|e| e.to_string())?;
            if agent::needs_confirmation(
                agent::TxKind::Native(agent::approx_units(&amount)),
                d.mainnet(),
            ) && !ask_approval(
                d,
                format!("send_native — {amount} {}", d.net.symbol),
                vec![
                    (
                        t(d.lang, Key::amount).into(),
                        format!("{amount} {}", d.net.symbol),
                    ),
                    (t(d.lang, Key::recipient).into(), to.to_checksummed()),
                    (t(d.lang, Key::network).into(), d.net.name.clone()),
                ],
            )
            .await
            {
                return Ok(agent::DECLINED.into());
            }
            run_tx(d, Some(to.clone()), units, Vec::new(), 30_000, fx).await
        }
        "send_token" => {
            let token = resolve_token(d, &agent::arg_str(args, "asset")?).await?;
            let to = agent::arg_address(args, "to")?;
            let amount = agent::arg_amount(args, "amount")?;
            let units = parse_amount(&amount, token.decimals).map_err(|e| e.to_string())?;
            if agent::needs_confirmation(agent::TxKind::Token, d.mainnet())
                && !ask_approval(
                    d,
                    format!("send_token — {amount} {}", token.symbol),
                    vec![
                        (
                            t(d.lang, Key::amount).into(),
                            format!("{amount} {}", token.symbol),
                        ),
                        (t(d.lang, Key::recipient).into(), to.to_checksummed()),
                        (t(d.lang, Key::network).into(), d.net.name.clone()),
                    ],
                )
                .await
            {
                return Ok(agent::DECLINED.into());
            }
            let contract = WalletAddress::new(&token.contract).map_err(|_| "bad token contract")?;
            run_tx(
                d,
                Some(contract),
                0,
                pocketskynet_core::chain::erc20_transfer_data(&to, units),
                100_000,
                fx,
            )
            .await
        }
        "approve_token" => {
            let token = resolve_token(d, &agent::arg_str(args, "asset")?).await?;
            let spender = agent::arg_address(args, "spender")?;
            let amount = agent::arg_amount(args, "amount")?;
            let units = parse_amount(&amount, token.decimals).map_err(|e| e.to_string())?;
            if agent::needs_confirmation(agent::TxKind::Token, d.mainnet())
                && !ask_approval(
                    d,
                    format!("approve_token — {amount} {}", token.symbol),
                    vec![
                        (
                            t(d.lang, Key::amount).into(),
                            format!("{amount} {}", token.symbol),
                        ),
                        ("Spender".into(), spender.to_checksummed()),
                        (t(d.lang, Key::network).into(), d.net.name.clone()),
                    ],
                )
                .await
            {
                return Ok(agent::DECLINED.into());
            }
            let contract = WalletAddress::new(&token.contract).map_err(|_| "bad token contract")?;
            run_tx(
                d,
                Some(contract),
                0,
                bank::erc20_approve_data(&spender, units),
                bank::GAS_FALLBACK_APPROVE,
                fx,
            )
            .await
        }
        "transfer_from_token" => {
            let token = resolve_token(d, &agent::arg_str(args, "asset")?).await?;
            let from = agent::arg_address(args, "from")?;
            let to = agent::arg_address(args, "to")?;
            let amount = agent::arg_amount(args, "amount")?;
            let units = parse_amount(&amount, token.decimals).map_err(|e| e.to_string())?;
            if agent::needs_confirmation(agent::TxKind::Token, d.mainnet())
                && !ask_approval(
                    d,
                    format!("transfer_from_token — {amount} {}", token.symbol),
                    vec![
                        (
                            t(d.lang, Key::amount).into(),
                            format!("{amount} {}", token.symbol),
                        ),
                        (t(d.lang, Key::from_label).into(), from.to_checksummed()),
                        (t(d.lang, Key::recipient).into(), to.to_checksummed()),
                    ],
                )
                .await
            {
                return Ok(agent::DECLINED.into());
            }
            let contract = WalletAddress::new(&token.contract).map_err(|_| "bad token contract")?;
            let data = abi::encode_call(
                "transferFrom(address,address,uint256)",
                &[
                    abi::Arg::Address(from),
                    abi::Arg::Address(to),
                    abi::Arg::Uint(units),
                ],
            );
            run_tx(d, Some(contract), 0, data, 120_000, fx).await
        }
        "greeter_set" => {
            let address = agent::arg_address(args, "address")?;
            let message = agent::arg_str(args, "message")?;
            if message.chars().count() > 280 {
                return Err("greeting is over 280 characters".into());
            }
            if agent::needs_confirmation(agent::TxKind::Token, d.mainnet())
                && !ask_approval(
                    d,
                    "greeter_set".into(),
                    vec![
                        ("Greeter".into(), address.to_checksummed()),
                        ("Message".into(), message.clone()),
                    ],
                )
                .await
            {
                return Ok(agent::DECLINED.into());
            }
            run_tx(
                d,
                Some(address),
                0,
                bank::set_greeting_data(&message),
                bank::GAS_FALLBACK_GREETER_SET,
                fx,
            )
            .await
        }
        "deploy_greeter" => {
            let message = agent::arg_str(args, "message")?;
            let data = bank::greeter_deploy_data(&message).map_err(|e| e.to_string())?;
            if agent::needs_confirmation(agent::TxKind::Token, d.mainnet())
                && !ask_approval(
                    d,
                    "deploy_greeter".into(),
                    vec![("Message".into(), message)],
                )
                .await
            {
                return Ok(agent::DECLINED.into());
            }
            let out = run_tx_full(d, None, 0, data, bank::GAS_FALLBACK_GREETER_DEPLOY, fx).await?;
            if let Some(address) = &out.contract_address {
                let chain_id = d.net.chain_id.unwrap_or_default();
                let mut list = saved_greeters(chain_id);
                list.push(address.clone());
                save_greeters(chain_id, &list);
                Ok(format!(
                    "greeter deployed at {address}, tx {} ({})",
                    out.tx_hash,
                    explorer_tx(&d.net, &out.tx_hash)
                ))
            } else {
                Ok(format!("broadcast, unconfirmed: tx {}", out.tx_hash))
            }
        }
        "deploy_token" => {
            let name_arg = agent::arg_str(args, "name")?;
            let symbol = agent::arg_str(args, "symbol")?.to_uppercase();
            let decimals: u8 = agent::arg_str(args, "decimals")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(18)
                .min(18);
            let supply_str = agent::arg_amount(args, "supply")?;
            let supply = parse_amount(&supply_str, decimals).map_err(|e| e.to_string())?;
            if agent::needs_confirmation(agent::TxKind::Token, d.mainnet())
                && !ask_approval(
                    d,
                    format!("deploy_token — {symbol}"),
                    vec![
                        (t(d.lang, Key::token_name).into(), name_arg.clone()),
                        (t(d.lang, Key::token_symbol).into(), symbol.clone()),
                        (t(d.lang, Key::initial_supply).into(), supply_str.clone()),
                    ],
                )
                .await
            {
                return Ok(agent::DECLINED.into());
            }
            let data = bank::erc20_deploy_data(&name_arg, &symbol, decimals, supply)
                .map_err(|e| e.to_string())?;
            let out = run_tx_full(d, None, 0, data, bank::GAS_FALLBACK_ERC20_DEPLOY, fx).await?;
            if let Some(address) = &out.contract_address {
                let chain_id = d.net.chain_id.unwrap_or_default();
                let mut list = custom_tokens(chain_id);
                list.push(Token {
                    symbol: symbol.clone(),
                    name: name_arg,
                    contract: address.clone(),
                    decimals,
                });
                save_custom_tokens(chain_id, &list);
                Ok(format!(
                    "{symbol} deployed at {address}, tx {} ({})",
                    out.tx_hash,
                    explorer_tx(&d.net, &out.tx_hash)
                ))
            } else {
                Ok(format!("broadcast, unconfirmed: tx {}", out.tx_hash))
            }
        }
        "import_token" => {
            d.stage.emit(Stage::Reading(name.into()));
            let address = agent::arg_address(args, "address")?;
            let token = fetch_token_meta(d, &address.as_str().to_lowercase()).await?;
            let chain_id = d.net.chain_id.unwrap_or_default();
            let mut list = custom_tokens(chain_id);
            if !list
                .iter()
                .any(|t| t.contract.eq_ignore_ascii_case(&token.contract))
            {
                list.push(token.clone());
                save_custom_tokens(chain_id, &list);
            }
            Ok(format!(
                "imported {} ({}) with {} decimals",
                agent::sanitize_onchain_text(&token.symbol, 20),
                agent::sanitize_onchain_text(&token.name, 48),
                token.decimals
            ))
        }
        "swap_tokens" => {
            let (from, to, amount_in, q) = quote_for_agent(d, args).await?;
            let slippage =
                agent::arg_str_opt(args, "slippagePercent").unwrap_or_else(|| "0.5".into());
            let bps = bank::slippage_bps(&slippage);
            let min_out = if q.wrap {
                q.amount_out
            } else {
                bank::apply_slippage_bps(q.amount_out, bps)
            };
            // A swap ALWAYS stops here — the policy function says so for
            // every chain, and going through it keeps the rule in one place.
            if agent::needs_confirmation(agent::TxKind::Swap, d.mainnet())
                && !ask_approval(
                    d,
                    format!(
                        "swap_tokens — {} {} → {}",
                        format_amount(amount_in, from.1),
                        from.0,
                        to.0
                    ),
                    vec![
                        (
                            t(d.lang, Key::from_label).into(),
                            format!("{} {}", format_amount(amount_in, from.1), from.0),
                        ),
                        (
                            t(d.lang, Key::send_to).into(),
                            format!("≈ {} {}", format_amount(q.amount_out, to.1), to.0),
                        ),
                        (
                            t(d.lang, Key::slippage_pct).into(),
                            format!("{slippage}% (min {})", format_amount(min_out, to.1)),
                        ),
                    ],
                )
                .await
            {
                return Ok(agent::DECLINED.into());
            }
            execute_swap(d, &q, amount_in, min_out, fx).await
        }

        // ---------------------------------------------------------------- fun --
        "generate_image" => {
            let prompt = agent::arg_str(args, "prompt")?;
            if prompt.chars().count() > 600 {
                return Err("prompt is over 600 characters".into());
            }
            let Some((provider, key)) = d.image_provider.clone() else {
                return Err("no image-capable AI provider key is configured".into());
            };
            d.stage.emit(Stage::Generating);
            let out = ai::generate_image(provider, &key, &prompt).await?;
            let url = match out {
                ai::ImageOut::Url(u) => u,
                ai::ImageOut::Bytes { mime, bytes } => d
                    .client
                    .upload_image(&mime, bytes)
                    .await
                    .map_err(|e| e.user_message())?,
            };
            fx.images.push(url.clone());
            Ok(format!("image generated and shown to the user ({url})"))
        }

        // Feeding the canonical list back is what lets a model that misspelt
        // a tool correct itself on the next round instead of retrying blind.
        other => Err(format!(
            "unknown tool \"{other}\" — the tools are: {}",
            agent::TOOL_NAMES.join(", ")
        )),
    }
}

// ------------------------------------------------------------- swap plumbing --

struct AgentQuote {
    amount_out: u128,
    path: Vec<WalletAddress>,
    wrap: bool,
    native_in: bool,
    native_out: bool,
    from_contract: Option<String>,
}

/// Resolve `from`/`to`/`amount` args and produce a quote. Returns
/// `((from_symbol, from_decimals), (to_symbol, to_decimals), amount_in, quote)`.
async fn quote_for_agent(
    d: &Deps,
    args: &Value,
) -> Result<((String, u8), (String, u8), u128, AgentQuote), String> {
    if d.net.chain_id != Some(bank::VVS_CHAIN_ID) {
        return Err(
            "swaps run on VVS Finance, Cronos mainnet only — switch the Bank to Mainnet".into(),
        );
    }
    let resolve_leg = |raw: String| async move {
        if raw.eq_ignore_ascii_case(&d.net.symbol) || raw.eq_ignore_ascii_case("native") {
            Ok::<_, String>((d.net.symbol.clone(), d.net.decimals, None))
        } else {
            let t = resolve_token(d, &raw).await?;
            Ok((t.symbol, t.decimals, Some(t.contract)))
        }
    };
    let from = resolve_leg(agent::arg_str(args, "from")?).await?;
    let to = resolve_leg(agent::arg_str(args, "to")?).await?;
    let amount = agent::arg_amount(args, "amount")?;
    let amount_in = parse_amount(&amount, from.1).map_err(|e| e.to_string())?;

    let is_wcro = |c: &Option<String>| {
        c.as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(bank::WCRO_CRONOS_MAINNET))
    };
    // Native↔WCRO is a 1:1 wrap — no router, no quote.
    if (from.2.is_none() && is_wcro(&to.2)) || (to.2.is_none() && is_wcro(&from.2)) {
        return Ok((
            (from.0.clone(), from.1),
            (to.0.clone(), to.1),
            amount_in,
            AgentQuote {
                amount_out: amount_in,
                path: Vec::new(),
                wrap: true,
                native_in: from.2.is_none(),
                native_out: to.2.is_none(),
                from_contract: from.2,
            },
        ));
    }

    let rpc = EvmRpc::new(&d.net.rpc_url);
    let wcro = WalletAddress::new(bank::WCRO_CRONOS_MAINNET).unwrap();
    let leg = |c: &Option<String>| {
        WalletAddress::new(c.as_deref().unwrap_or(bank::WCRO_CRONOS_MAINNET)).unwrap()
    };
    let direct = vec![leg(&from.2), leg(&to.2)];
    let mut chosen: Option<(Vec<WalletAddress>, u128)> = None;
    for path in [direct.clone(), vec![leg(&from.2), wcro.clone(), leg(&to.2)]] {
        if path.len() == 3 && (path[0] == wcro || path[2] == wcro) {
            continue;
        }
        let data = format!(
            "0x{}",
            hex::encode(bank::get_amounts_out_data(amount_in, &path))
        );
        if let Ok(out_hex) = rpc.eth_call(bank::VVS_ROUTER_CRONOS_MAINNET, &data).await {
            if let Ok(amounts) = abi::decode_uint_array(&out_hex) {
                if let Some(&last) = amounts.last() {
                    chosen = Some((path, last));
                    break;
                }
            }
        }
    }
    let (path, amount_out) = chosen.ok_or("no route found for this pair")?;
    Ok((
        (from.0.clone(), from.1),
        (to.0.clone(), to.1),
        amount_in,
        AgentQuote {
            amount_out,
            path,
            wrap: false,
            native_in: from.2.is_none(),
            native_out: to.2.is_none(),
            from_contract: from.2,
        },
    ))
}

async fn execute_swap(
    d: &Deps,
    q: &AgentQuote,
    amount_in: u128,
    min_out: u128,
    fx: &mut ToolFx<'_>,
) -> Result<String, String> {
    let keys = d.keys()?;
    if q.wrap {
        let wcro = WalletAddress::new(bank::WCRO_CRONOS_MAINNET).unwrap();
        let (value, data) = if q.native_in {
            (amount_in, abi::encode_call("deposit()", &[]))
        } else {
            (
                0,
                abi::encode_call("withdraw(uint256)", &[abi::Arg::Uint(amount_in)]),
            )
        };
        return run_tx(d, Some(wcro), value, data, 100_000, fx).await;
    }

    let router = WalletAddress::new(bank::VVS_ROUTER_CRONOS_MAINNET).unwrap();
    if let Some(contract) = &q.from_contract {
        // Router allowance first.
        let out = erc20_read(d, contract, bank::erc20_allowance_data(&d.me, &router)).await?;
        let allowance = abi::decode_uint(&out, 0).unwrap_or(0);
        if allowance < amount_in {
            let token = WalletAddress::new(contract).map_err(|_| "bad token address")?;
            d.stage.emit(Stage::Sending);
            send_contract_tx(
                d.lang,
                &d.net,
                &d.me,
                keys.clone(),
                Some(token),
                0,
                bank::erc20_approve_data(&router, amount_in),
                bank::GAS_FALLBACK_APPROVE,
            )
            .await?;
        }
    }

    let deadline = super::bank::deadline_in_20_minutes();
    let (value, data) = if q.native_in {
        (
            amount_in,
            bank::swap_exact_eth_for_tokens_data(min_out, &q.path, &d.me, deadline),
        )
    } else if q.native_out {
        (
            0,
            bank::swap_exact_tokens_for_eth_data(amount_in, min_out, &q.path, &d.me, deadline),
        )
    } else {
        (
            0,
            bank::swap_exact_tokens_for_tokens_data(amount_in, min_out, &q.path, &d.me, deadline),
        )
    };
    run_tx(d, Some(router), value, data, bank::GAS_FALLBACK_SWAP, fx).await
}

// ------------------------------------------------------------------ tx runner --

async fn run_tx_full(
    d: &Deps,
    to: Option<WalletAddress>,
    value: u128,
    data: Vec<u8>,
    gas_fallback: u128,
    fx: &mut ToolFx<'_>,
) -> Result<super::bank::TxDone, String> {
    let keys = d.keys()?;
    d.stage.emit(Stage::Sending);
    let on_phase = {
        let stage = d.stage.clone();
        Callback::from(move |p: super::burst::TxPhase| {
            stage.emit(match p {
                super::burst::TxPhase::Confirm => Stage::Confirming,
                _ => Stage::Sending,
            });
        })
    };
    let done = super::bank::send_contract_tx_with(
        d.lang,
        &d.net,
        &d.me,
        keys,
        to,
        value,
        data,
        gas_fallback,
        Some(on_phase),
    )
    .await?;
    fx.tx_hashes.push(done.tx_hash.clone());
    Ok(done)
}

async fn run_tx(
    d: &Deps,
    to: Option<WalletAddress>,
    value: u128,
    data: Vec<u8>,
    gas_fallback: u128,
    fx: &mut ToolFx<'_>,
) -> Result<String, String> {
    let done = run_tx_full(d, to, value, data, gas_fallback, fx).await?;
    Ok(format!(
        "confirmed, tx {} ({})",
        done.tx_hash,
        explorer_tx(&d.net, &done.tx_hash)
    ))
}

// ---------------------------------------------------------------- turn loop --

struct TurnOut {
    text: String,
    tools: Vec<String>,
    images: Vec<String>,
    tx_hashes: Vec<String>,
}

async fn run_turn(d: Deps, system: String, mut turns: Vec<ChatTurn>) -> TurnOut {
    let mut tools = Vec::new();
    let mut images = Vec::new();
    let mut tx_hashes = Vec::new();
    for _ in 0..agent::MAX_TOOL_ROUNDS {
        d.stage.emit(Stage::Thinking);
        let raw = match ai::generate_chat(d.provider, &d.key, &system, &turns).await {
            Ok(r) => r,
            Err(e) => {
                return TurnOut {
                    text: e,
                    tools,
                    images,
                    tx_hashes,
                }
            }
        };
        match agent::parse_reply(&raw) {
            agent::Reply::Text(text) => {
                return TurnOut {
                    text,
                    tools,
                    images,
                    tx_hashes,
                }
            }
            agent::Reply::Tool { name, args } => {
                tools.push(name.clone());
                let mut fx = ToolFx {
                    images: &mut images,
                    tx_hashes: &mut tx_hashes,
                };
                let result = match exec_tool(&d, &name, &args, &mut fx).await {
                    Ok(s) => s,
                    Err(e) => format!("ERROR: {e}"),
                };
                turns.push(ChatTurn {
                    user: false,
                    content: raw,
                });
                turns.push(ChatTurn {
                    user: true,
                    content: format!("[TOOL RESULT {name}] {result}"),
                });
            }
        }
    }
    TurnOut {
        text: t(d.lang, Key::banker_out_of_steam).to_owned(),
        tools,
        images,
        tx_hashes,
    }
}

// ---------------------------------------------------------------- component --

#[derive(Properties, PartialEq)]
pub struct BankerProps {
    pub net: Network,
    pub me: WalletAddress,
    /// Fired after any transaction so the surrounding page can refresh.
    pub on_mutation: Callback<()>,
}

#[function_component(BankerView)]
pub fn banker_view(p: &BankerProps) -> Html {
    let store = use_store();
    let lang = store.language;
    let log = use_state(load_log);
    let draft = use_state(String::new);
    let busy = use_state(|| false);
    let stage = use_state(|| Option::<Stage>::None);
    let approval = use_state(|| Option::<Approval>::None);
    let chat_ref = use_node_ref();

    let settings = crate::ai::AiSettings::load();
    let provider = settings.text_provider();

    // Keep the newest message in view.
    {
        let chat_ref = chat_ref.clone();
        use_effect_with((log.len(), stage.is_some()), move |_| {
            if let Some(el) = chat_ref.cast::<web_sys::Element>() {
                el.set_scroll_top(el.scroll_height());
            }
            || ()
        });
    }

    let ask = {
        let net = p.net.clone();
        let me = p.me.clone();
        let on_mutation = p.on_mutation.clone();
        let log = log.clone();
        let draft = draft.clone();
        let busy = busy.clone();
        let stage = stage.clone();
        let approval = approval.clone();
        let settings = settings.clone();
        let store = store.clone();
        Callback::from(move |_: MouseEvent| {
            let question = draft.trim().to_owned();
            if question.is_empty() || *busy {
                return;
            }
            let Some(provider) = settings.text_provider() else {
                return;
            };
            let key = settings.key_for(provider).unwrap_or_default().to_owned();
            let image_provider = settings
                .image_provider()
                .and_then(|p| settings.key_for(p).map(|k| (p, k.to_owned())));

            // Record the question.
            let mut next = (*log).clone();
            next.push(agent::StoredMsg {
                user: true,
                text: question.clone(),
                tools: vec![],
                images: vec![],
                ts: js_sys::Date::now(),
            });
            agent::cap_history(&mut next);
            save_log(&next);
            log.set(next.clone());
            draft.set(String::new());
            busy.set(true);

            // Replay the newest context window, then the fresh question.
            let mut turns: Vec<ChatTurn> = next
                .iter()
                .rev()
                .take(agent::CONTEXT_MESSAGES)
                .rev()
                .map(|m| ChatTurn {
                    user: m.user,
                    content: m.text.clone(),
                })
                .collect();
            if turns.last().map(|t| t.content != question).unwrap_or(true) {
                turns.push(ChatTurn {
                    user: true,
                    content: question.clone(),
                });
            }

            let deps = Deps {
                lang: store.language,
                provider,
                key,
                image_provider,
                net: net.clone(),
                me: me.clone(),
                keys: store.auth.session().map(|s| s.keys.clone()),
                client: store.client.clone(),
                stage: {
                    let stage = stage.clone();
                    Callback::from(move |s: Stage| stage.set(Some(s)))
                },
                approve: {
                    let approval = approval.clone();
                    Callback::from(move |a: Approval| approval.set(Some(a)))
                },
            };

            // The live context the prompt opens with.
            let tokens = deps
                .all_tokens()
                .iter()
                .map(|t| agent::TokenLine {
                    symbol: t.symbol.clone(),
                    name: t.name.clone(),
                    contract: t.contract.clone(),
                    decimals: t.decimals,
                    balance: None,
                })
                .collect();
            let cx = agent::AgentContext {
                network_name: net.name.clone(),
                chain_id: net.chain_id.unwrap_or_default(),
                symbol: net.symbol.clone(),
                mainnet: !net.testnet,
                address: me.to_checksummed(),
                native_balance: None,
                tokens,
                greeters: saved_greeters(net.chain_id.unwrap_or_default()),
            };

            let log = log.clone();
            let busy = busy.clone();
            let stage = stage.clone();
            let approval = approval.clone();
            let on_mutation = on_mutation.clone();
            let store = store.clone();
            // `next` rides into the async block: reading `*log` there would
            // yield the render-time snapshot, which does not yet contain the
            // question we just pushed — the user's bubble would vanish.
            let log_after_question = next;
            crate::progression::award(pocketskynet_core::progression::Award::AgentQueried);
            wasm_bindgen_futures::spawn_local(async move {
                // Fetch the native balance for the prompt — cheap, and it lets
                // "what's my balance" answer without a tool round.
                let mut cx = cx;
                let rpc = EvmRpc::new(&deps.net.rpc_url);
                if let Ok(b) = rpc.balance(&deps.me).await {
                    cx.native_balance = Some(format!(
                        "{} {}",
                        format_amount(b, deps.net.decimals),
                        deps.net.symbol
                    ));
                }
                let system = agent::system_prompt(&cx);

                let out = run_turn(deps, system, turns).await;

                let mut next = log_after_question;
                next.push(agent::StoredMsg {
                    user: false,
                    text: out.text,
                    tools: out.tools,
                    images: out.images,
                    ts: js_sys::Date::now(),
                });
                agent::cap_history(&mut next);
                save_log(&next);
                log.set(next);
                stage.set(None);
                approval.set(None);
                busy.set(false);
                if !out.tx_hashes.is_empty() {
                    on_mutation.emit(());
                    super::burst::burst_from_selector(
                        ".fn-banker__input",
                        super::burst::Variant::Pop,
                        16,
                    );
                    toast::success(&store, t(store.language, Key::transaction_confirmed));
                }
            });
        })
    };

    let export = |ext: &'static str, log: &UseStateHandle<Vec<agent::StoredMsg>>| {
        let log = log.clone();
        Callback::from(move |_: MouseEvent| {
            let body = if ext == "csv" {
                let mut s = String::from("timestamp,role,text\n");
                for m in log.iter() {
                    let quoted = m.text.replace('"', "\"\"");
                    s.push_str(&format!(
                        "{},{},\"{}\"\n",
                        m.ts,
                        if m.user { "user" } else { "agent" },
                        quoted
                    ));
                }
                s
            } else {
                log.iter()
                    .filter_map(|m| serde_json::to_string(m).ok())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            if let Some(url) = super::common::object_url(body.as_bytes(), "text/plain") {
                super::common::save_as(&url, &format!("ai-banker-history.{ext}"));
            }
        })
    };

    let clear = {
        let log = log.clone();
        Callback::from(move |_: MouseEvent| {
            use gloo_storage::Storage;
            gloo_storage::LocalStorage::delete(BANKER_LOG_KEY);
            log.set(Vec::new());
        })
    };

    let suggestions: [Key; 5] = [
        Key::banker_sug_balance,
        Key::banker_sug_gas,
        Key::banker_sug_deploy,
        Key::banker_sug_swap,
        Key::banker_sug_image,
    ];

    html! {
        <div class="fn-stack fn-bank__pane fn-banker">
            if provider.is_none() {
                <div class="fn-banner" role="note">{ t(lang, Key::banker_needs_key) }</div>
            } else {
                <div class="fn-row fn-banker__toolbar">
                    <span class="fn-grow fn-muted">{ t(lang, Key::banker_intro) }</span>
                    <button type="button" class="topcoat-button--quiet" disabled={log.is_empty()}
                            onclick={export("csv", &log)}>{ "CSV" }</button>
                    <button type="button" class="topcoat-button--quiet" disabled={log.is_empty()}
                            onclick={export("jsonl", &log)}>{ "JSONL" }</button>
                    <button type="button" class="topcoat-button--quiet" disabled={log.is_empty()}
                            onclick={clear}>{ t(lang, Key::banker_clear) }</button>
                </div>

                <div class="fn-bank__chat fn-banker__chat fn-scroll" ref={chat_ref}>
                    if log.is_empty() {
                        // The executing banker's face (`banker-core`) — the
                        // chrome endoskeleton in a suit, not the flat teller.
                        // Tap for the spotlight.
                        <button type="button" class="fn-spot__opener fn-banker__face-btn"
                            aria-label={t(lang, Key::ai_banker)}
                            onclick={Callback::from(move |_: MouseEvent| {
                                super::spotlight::show(super::spotlight::Spot {
                                    image: "/static/img/banker-core.png".into(),
                                    title: t(lang, Key::ai_banker).to_owned(),
                                    subtitle: None,
                                    copy: None,
                                    hue: 190,
                                });
                            })}
                        >
                            <div class="fn-art fn-art--banker-core fn-banker__face" aria-hidden="true"></div>
                        </button>
                        <div class="fn-banker__sugs">
                            { for suggestions.iter().map(|k| {
                                let draft = draft.clone();
                                let text = t(lang, *k);
                                html! {
                                    <button type="button" class="fn-banker__sug"
                                        onclick={Callback::from(move |_: MouseEvent|
                                            draft.set(text.to_owned()))}>
                                        { text }
                                    </button>
                                }
                            }) }
                        </div>
                    }
                    { for log.iter().enumerate().map(|(i, m)| html! {
                        <div key={i}
                             class={if m.user { "fn-banker__msg fn-bank__msg fn-bank__msg--user" }
                                    else { "fn-banker__msg fn-bank__msg" }}>
                            <span class="fn-banker__text">{ &m.text }</span>
                            if !m.images.is_empty() {
                                <span class="fn-banker__imgs">
                                    { for m.images.iter().map(|u| html! {
                                        <img class="fn-banker__img" src={u.clone()} alt="" loading="lazy" />
                                    }) }
                                </span>
                            }
                            if !m.tools.is_empty() {
                                <span class="fn-banker__chips">
                                    { for m.tools.iter().map(|name| html! {
                                        <code class="fn-banker__chip">{ name }</code>
                                    }) }
                                </span>
                            }
                        </div>
                    }) }
                    if let Some(s) = &*stage {
                        <div class="fn-bank__msg fn-banker__progress" role="status">
                            <span>{ s.label(lang) }</span>
                            <span class="fn-banker__bar" aria-hidden="true"></span>
                        </div>
                    }
                </div>

                <div class="fn-row">
                    <input
                        class="topcoat-text-input fn-grow fn-banker__input"
                        placeholder={t(lang, Key::banker_placeholder)}
                        aria-label={t(lang, Key::ai_banker)}
                        value={(*draft).clone()}
                        oninput={{
                            let draft = draft.clone();
                            Callback::from(move |e: InputEvent| {
                                if let Some(el) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                                    draft.set(el.value());
                                }
                            })
                        }}
                        onkeydown={{
                            let ask = ask.clone();
                            Callback::from(move |e: KeyboardEvent| {
                                if e.key() == "Enter" {
                                    e.prevent_default();
                                    ask.emit(MouseEvent::new("click").unwrap());
                                }
                            })
                        }}
                    />
                    <button type="button" class="topcoat-button--cta" disabled={*busy} onclick={ask}>
                        if *busy { <Spinner /> } else { { t(lang, Key::send_button) } }
                    </button>
                </div>
            }

            if let Some(a) = (*approval).clone() {
                <Dialog
                    title={t(lang, Key::banker_approve_title)}
                    danger=true
                    on_close={{
                        let approval = approval.clone();
                        let decision = a.decision.clone();
                        Callback::from(move |_: ()| {
                            *decision.borrow_mut() = Some(false);
                            approval.set(None);
                        })
                    }}
                    footer={{
                        let approve_btn = {
                            let approval = approval.clone();
                            let decision = a.decision.clone();
                            Callback::from(move |_: MouseEvent| {
                                *decision.borrow_mut() = Some(true);
                                approval.set(None);
                            })
                        };
                        let cancel_btn = {
                            let approval = approval.clone();
                            let decision = a.decision.clone();
                            Callback::from(move |_: MouseEvent| {
                                *decision.borrow_mut() = Some(false);
                                approval.set(None);
                            })
                        };
                        html! {
                            <>
                                <button type="button" class="topcoat-button" onclick={cancel_btn}>
                                    { t(lang, Key::cancel) }
                                </button>
                                <button type="button" class="topcoat-button--cta-danger topcoat-button--cta"
                                        onclick={approve_btn}>
                                    { t(lang, Key::banker_approve) }
                                </button>
                            </>
                        }
                    }}
                >
                    <div class="fn-stack">
                        <code class="fn-banker__approve-title">{ &a.title }</code>
                        <dl class="fn-banker__approve-lines">
                            { for a.lines.iter().map(|(k, v)| html! {
                                <>
                                    <dt>{ k }</dt>
                                    <dd class="fn-nums">{ v }</dd>
                                </>
                            }) }
                        </dl>
                    </div>
                </Dialog>
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_has_a_label_in_every_language() {
        use crate::i18n::Lang;
        let stages = [
            Stage::Thinking,
            Stage::Reading("get_gas_price".into()),
            Stage::Sending,
            Stage::Confirming,
            Stage::Generating,
        ];
        for lang in Lang::ALL {
            for stage in &stages {
                assert!(!stage.label(lang).is_empty(), "{stage:?} blank in {lang:?}");
            }
        }
        // The reading stage names the tool it is running — that suffix is the
        // difference between "working" and "working on what you asked".
        assert!(Stage::Reading("swap_quote".into())
            .label(Lang::En)
            .contains("swap_quote"));
    }

    #[test]
    fn an_approval_is_identified_by_its_decision_cell() {
        // The dialog re-renders while the tool future polls the cell; equality
        // by pointer is what keeps Yew from tearing down the open dialog.
        let decision = Rc::new(RefCell::new(None));
        let a = Approval {
            title: "send_native — 2 CRO".into(),
            lines: vec![],
            decision: decision.clone(),
        };
        let same_cell = Approval {
            title: "different title".into(),
            lines: vec![("Amount".into(), "2 CRO".into())],
            decision,
        };
        let other = Approval {
            title: "send_native — 2 CRO".into(),
            lines: vec![],
            decision: Rc::new(RefCell::new(None)),
        };
        assert_eq!(a, same_cell);
        assert_ne!(a, other);
    }

    #[test]
    fn the_stage_vocabulary_matches_the_relay_phases() {
        // run_tx_full maps burst::TxPhase into chat stages: Confirm becomes
        // Confirming, everything earlier reads as Sending. If TxPhase grows a
        // phase this match must be revisited — the map lives in run_tx_full,
        // this test pins the two ends it maps between.
        use super::super::burst::TxPhase;
        assert_eq!(TxPhase::ALL.len(), 4);
        let map = |p: TxPhase| match p {
            TxPhase::Confirm => Stage::Confirming,
            _ => Stage::Sending,
        };
        assert_eq!(map(TxPhase::Confirm), Stage::Confirming);
        for p in [TxPhase::Uplink, TxPhase::Sign, TxPhase::Broadcast] {
            assert_eq!(map(p), Stage::Sending);
        }
    }
}
