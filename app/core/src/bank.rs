//! The Bank's on-chain vocabulary: ERC-20 management, the Greeter contract,
//! and VVS Finance swaps — ported from the reference client's
//! `services/{erc20,greeter,vvs}.ts`, with ethers.js replaced by `abi.rs`.
//!
//! Everything here is pure calldata construction and arithmetic. The web
//! client owns the RPC round trips, wallclock deadlines, and persistence;
//! this module owns the bytes, so the bytes are what the tests pin.

use crate::abi::{self, Arg};
use crate::chain::{ChainError, Token};
use crate::ids::WalletAddress;

// ------------------------------------------------------------ VVS Finance --

/// VVS Finance router (Cronos mainnet). VVS deploys no testnet router — the
/// swap UI must gate on chain id 25, exactly as the reference client does.
pub const VVS_ROUTER_CRONOS_MAINNET: &str = "0x145863eb42cf62847a6ca784e6416c1682b1b2ae";
/// Wrapped CRO — the native leg of every router path.
pub const WCRO_CRONOS_MAINNET: &str = "0x5c7f8a570d578ed84e63fdfa7b1ee72deae1ae23";
/// The chain the router lives on.
pub const VVS_CHAIN_ID: u64 = 25;

/// The reference client's `KNOWN_TOKENS` for Cronos mainnet. The server's
/// registry carries only USDC; the Bank offers the fuller set, and imports
/// cover the rest.
pub fn known_tokens(chain_id: u64) -> Vec<Token> {
    if chain_id != VVS_CHAIN_ID {
        return Vec::new();
    }
    let t = |symbol: &str, name: &str, contract: &str, decimals: u8| Token {
        symbol: symbol.into(),
        name: name.into(),
        contract: contract.into(),
        decimals,
    };
    vec![
        t(
            "USDC",
            "USD Coin",
            "0xc21223249ca28397b4b6541dffaecc539bff0c59",
            6,
        ),
        t(
            "VVS",
            "VVS Finance",
            "0x2d03bece6747adc00e1a131bba1469c15fd11e03",
            18,
        ),
        t("WCRO", "Wrapped CRO", WCRO_CRONOS_MAINNET, 18),
        t(
            "USDT",
            "Tether USD",
            "0x66e428c3f67a68878562e79a0234c1f83c208770",
            6,
        ),
        t(
            "DAI",
            "Dai Stablecoin",
            "0xf2001b145b43032aaf5ee2884e456ccd805f677d",
            18,
        ),
        t(
            "WETH",
            "Wrapped Ether",
            "0xe44fd7fcb2b1581822d0c862b68222998a0c299a",
            18,
        ),
        t(
            "WBTC",
            "Wrapped BTC",
            "0x062e66477faf219f25d27dced647bf57c3107d52",
            8,
        ),
        t(
            "ATOM",
            "Cosmos Hub",
            "0xb888d8dd1733d72681b30c00ee76bde93ae7aa93",
            6,
        ),
    ]
}

/// `getAmountsOut(amountIn, path)` — the read-only quote.
pub fn get_amounts_out_data(amount_in: u128, path: &[WalletAddress]) -> Vec<u8> {
    abi::encode_call(
        "getAmountsOut(uint256,address[])",
        &[Arg::Uint(amount_in), Arg::Addresses(path.to_vec())],
    )
}

/// `swapExactETHForTokens(amountOutMin, path, to, deadline)` — the amount in
/// rides as the transaction's `value`.
pub fn swap_exact_eth_for_tokens_data(
    amount_out_min: u128,
    path: &[WalletAddress],
    to: &WalletAddress,
    deadline: u64,
) -> Vec<u8> {
    abi::encode_call(
        "swapExactETHForTokens(uint256,address[],address,uint256)",
        &[
            Arg::Uint(amount_out_min),
            Arg::Addresses(path.to_vec()),
            Arg::Address(to.clone()),
            Arg::Uint(deadline as u128),
        ],
    )
}

/// `swapExactTokensForETH(amountIn, amountOutMin, path, to, deadline)`.
pub fn swap_exact_tokens_for_eth_data(
    amount_in: u128,
    amount_out_min: u128,
    path: &[WalletAddress],
    to: &WalletAddress,
    deadline: u64,
) -> Vec<u8> {
    abi::encode_call(
        "swapExactTokensForETH(uint256,uint256,address[],address,uint256)",
        &[
            Arg::Uint(amount_in),
            Arg::Uint(amount_out_min),
            Arg::Addresses(path.to_vec()),
            Arg::Address(to.clone()),
            Arg::Uint(deadline as u128),
        ],
    )
}

/// `swapExactTokensForTokens(amountIn, amountOutMin, path, to, deadline)`.
pub fn swap_exact_tokens_for_tokens_data(
    amount_in: u128,
    amount_out_min: u128,
    path: &[WalletAddress],
    to: &WalletAddress,
    deadline: u64,
) -> Vec<u8> {
    abi::encode_call(
        "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
        &[
            Arg::Uint(amount_in),
            Arg::Uint(amount_out_min),
            Arg::Addresses(path.to_vec()),
            Arg::Address(to.clone()),
            Arg::Uint(deadline as u128),
        ],
    )
}

/// `amountOutMin` after slippage, in basis points — the reference's
/// `applySlippage`, integer arithmetic: `amount × (10000 − bps) / 10000`.
pub fn apply_slippage_bps(amount: u128, bps: u32) -> u128 {
    let bps = bps.min(10_000) as u128;
    amount.saturating_mul(10_000 - bps) / 10_000
}

/// Parse a slippage percentage string to basis points, clamped to the
/// reference's `[0.1%, 50%]`, defaulting to 0.5% on garbage.
pub fn slippage_bps(input: &str) -> u32 {
    let pct: f64 = input.trim().parse().unwrap_or(0.5);
    let pct = pct.clamp(0.1, 50.0);
    (pct * 100.0).round() as u32
}

// ----------------------------------------------------------------- ERC-20 --

pub fn erc20_approve_data(spender: &WalletAddress, amount: u128) -> Vec<u8> {
    abi::encode_call(
        "approve(address,uint256)",
        &[Arg::Address(spender.clone()), Arg::Uint(amount)],
    )
}

pub fn erc20_allowance_data(owner: &WalletAddress, spender: &WalletAddress) -> Vec<u8> {
    abi::encode_call(
        "allowance(address,address)",
        &[Arg::Address(owner.clone()), Arg::Address(spender.clone())],
    )
}

pub fn erc20_symbol_data() -> Vec<u8> {
    abi::encode_call("symbol()", &[])
}

pub fn erc20_name_data() -> Vec<u8> {
    abi::encode_call("name()", &[])
}

pub fn erc20_decimals_data() -> Vec<u8> {
    abi::encode_call("decimals()", &[])
}

/// Deploy calldata for the reference's ERC-20 (solc 0.8.24): creation
/// bytecode + `constructor(string,string,uint8,uint256)` arguments. The
/// initial supply is in base units (already scaled by `decimals`).
pub fn erc20_deploy_data(
    name: &str,
    symbol: &str,
    decimals: u8,
    initial_supply_units: u128,
) -> Result<Vec<u8>, ChainError> {
    let mut data = hex::decode(ERC20_BYTECODE_HEX.trim_start_matches("0x"))
        .map_err(|_| ChainError::InvalidQuantity)?;
    data.extend(abi::encode_args(&[
        Arg::Str(name.to_owned()),
        Arg::Str(symbol.to_owned()),
        Arg::Uint(decimals as u128),
        Arg::Uint(initial_supply_units),
    ]));
    Ok(data)
}

// ---------------------------------------------------------------- Greeter --

pub fn greet_data() -> Vec<u8> {
    abi::encode_call("greet()", &[])
}

pub fn greeter_owner_data() -> Vec<u8> {
    abi::encode_call("owner()", &[])
}

pub fn set_greeting_data(message: &str) -> Vec<u8> {
    abi::encode_call("setGreeting(string)", &[Arg::Str(message.to_owned())])
}

/// Deploy calldata for the reference's Greeter: creation bytecode +
/// `constructor(string)` argument.
pub fn greeter_deploy_data(initial_greeting: &str) -> Result<Vec<u8>, ChainError> {
    let mut data = hex::decode(GREETER_BYTECODE_HEX.trim_start_matches("0x"))
        .map_err(|_| ChainError::InvalidQuantity)?;
    data.extend(abi::encode_args(&[Arg::Str(initial_greeting.to_owned())]));
    Ok(data)
}

/// Creation bytecode, byte-identical to the reference client's constants —
/// a contract deployed from either client verifies as the same code.
const GREETER_BYTECODE_HEX: &str = include_str!("contracts/greeter.hex");
const ERC20_BYTECODE_HEX: &str = include_str!("contracts/erc20.hex");

/// Gas-limit fallbacks when `eth_estimateGas` fails, from the reference's
/// `GAS_UNITS` table (with its own headroom kept).
pub const GAS_FALLBACK_APPROVE: u128 = 80_000;
pub const GAS_FALLBACK_SWAP: u128 = 300_000;
pub const GAS_FALLBACK_ERC20_DEPLOY: u128 = 1_400_000;
pub const GAS_FALLBACK_GREETER_DEPLOY: u128 = 1_200_000;
pub const GAS_FALLBACK_GREETER_SET: u128 = 90_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(tail: &str) -> WalletAddress {
        WalletAddress::new(&format!("0x{:0>40}", tail)).unwrap()
    }

    #[test]
    fn the_router_and_wcro_addresses_are_wellformed_and_distinct() {
        for a in [VVS_ROUTER_CRONOS_MAINNET, WCRO_CRONOS_MAINNET] {
            assert!(WalletAddress::new(a).is_ok(), "{a}");
        }
        assert_ne!(VVS_ROUTER_CRONOS_MAINNET, WCRO_CRONOS_MAINNET);
    }

    #[test]
    fn known_tokens_exist_only_on_mainnet_and_parse_as_addresses() {
        assert!(known_tokens(338).is_empty());
        let tokens = known_tokens(25);
        assert_eq!(tokens.len(), 8);
        assert_eq!(tokens[0].symbol, "USDC");
        assert_eq!(tokens[0].decimals, 6);
        for t in &tokens {
            assert!(WalletAddress::new(&t.contract).is_ok(), "{}", t.symbol);
        }
        // WCRO in the list is the same WCRO the router paths use.
        assert!(tokens.iter().any(|t| t.contract == WCRO_CRONOS_MAINNET));
    }

    #[test]
    fn a_quote_call_carries_the_selector_and_path() {
        let data = get_amounts_out_data(1_000_000, &[addr("aa"), addr("bb")]);
        assert_eq!(hex::encode(&data[..4]), "d06ca61f");
        assert_eq!(data.len(), 4 + 32 * 5);
    }

    #[test]
    fn each_swap_shape_uses_its_own_selector() {
        let path = [addr("aa"), addr("bb")];
        let me = addr("cc");
        let eth_in = swap_exact_eth_for_tokens_data(1, &path, &me, 99);
        let eth_out = swap_exact_tokens_for_eth_data(2, 1, &path, &me, 99);
        let tok_tok = swap_exact_tokens_for_tokens_data(2, 1, &path, &me, 99);
        assert_eq!(hex::encode(&eth_in[..4]), "7ff36ab5");
        assert_eq!(hex::encode(&eth_out[..4]), "18cbafe5");
        assert_eq!(hex::encode(&tok_tok[..4]), "38ed1739");
        // Token-in shapes carry one more head word than the ETH-in shape.
        assert_eq!(eth_out.len(), eth_in.len() + 32);
        assert_eq!(tok_tok.len(), eth_out.len());
    }

    #[test]
    fn slippage_arithmetic_matches_the_reference() {
        assert_eq!(apply_slippage_bps(10_000, 50), 9_950); // 0.5%
        assert_eq!(apply_slippage_bps(10_000, 0), 10_000);
        assert_eq!(apply_slippage_bps(10_000, 10_000), 0);
        // Absurd magnitudes saturate instead of panicking, and can only
        // shrink the minimum — the safe direction for a minimum.
        assert!(apply_slippage_bps(u128::MAX, 50) < u128::MAX);
    }

    #[test]
    fn slippage_parsing_clamps_like_the_reference() {
        assert_eq!(slippage_bps("0.5"), 50);
        assert_eq!(slippage_bps("50"), 5_000);
        assert_eq!(slippage_bps("99"), 5_000); // clamp high
        assert_eq!(slippage_bps("0"), 10); // clamp low to 0.1%
        assert_eq!(slippage_bps("nonsense"), 50); // default 0.5%
    }

    #[test]
    fn approve_and_allowance_encode_statically() {
        let spender = addr("11");
        let owner = addr("22");
        let approve = erc20_approve_data(&spender, 7);
        assert_eq!(hex::encode(&approve[..4]), "095ea7b3");
        assert_eq!(approve.len(), 4 + 64);
        let allowance = erc20_allowance_data(&owner, &spender);
        assert_eq!(hex::encode(&allowance[..4]), "dd62ed3e");
        assert_eq!(allowance.len(), 4 + 64);
    }

    #[test]
    fn deploy_data_is_bytecode_plus_constructor_args() {
        let greeter = greeter_deploy_data("hi").unwrap();
        let bytecode_len = (GREETER_BYTECODE_HEX.len() - 2) / 2;
        // string arg: offset + length + 1 padded word
        assert_eq!(greeter.len(), bytecode_len + 32 * 3);
        assert_eq!(&greeter[..4], &hex::decode("60806040").unwrap()[..]);

        let token = erc20_deploy_data("My Token", "MTK", 18, 1_000).unwrap();
        let erc20_len = (ERC20_BYTECODE_HEX.len() - 2) / 2;
        // (string,string,uint8,uint256): 4 head words + 2×(len+data)
        assert_eq!(token.len(), erc20_len + 32 * 8);
        // solc metadata tail is intact right before the args.
        assert_eq!(hex::encode(&token[erc20_len - 2..erc20_len]), "0033");
    }

    #[test]
    fn greeter_read_and_write_calls_encode() {
        assert_eq!(hex::encode(greet_data()), "cfae3217");
        assert_eq!(hex::encode(greeter_owner_data()), "8da5cb5b");
        let set = set_greeting_data("안녕하세요");
        assert_eq!(hex::encode(&set[..4]), "a4136862");
        // Korean is multi-byte; the length word counts bytes, not chars.
        let expected_len = "안녕하세요".len();
        assert_eq!(
            crate::abi::decode_uint(&format!("0x{}", hex::encode(&set[36..68])), 0).unwrap(),
            expected_len as u128
        );
    }
}
