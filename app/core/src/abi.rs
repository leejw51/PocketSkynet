//! Minimal Solidity ABI encoding and decoding — exactly the surface the Bank
//! needs, written out rather than pulled from an ABI crate.
//!
//! `chain.rs` hardcodes two selectors and says a full ABI encoder would be
//! 100× the code. The Bank (docs/SEARCH.md's sibling in ambition: ERC-20
//! management, a Greeter contract, VVS swaps) needs dynamic types — `string`
//! arguments, `address[]` paths, `uint256[]` returns — which is past the
//! point where hardcoding stays honest, but still far short of needing a
//! general encoder. This module covers: static words (uint, address),
//! dynamic strings and address arrays, the head/tail layout that mixes them,
//! and the three return shapes the Bank reads (uint word, string, uint[]).
//!
//! Amounts are `u128` throughout, like the rest of `chain.rs`: 3.4×10³⁸ is
//! beyond any real balance, and a `uint256` that exceeds it is an error worth
//! surfacing rather than silently truncating.

use sha3::{Digest, Keccak256};

use crate::chain::ChainError;
use crate::ids::WalletAddress;

/// Keccak-256 — the EVM's hash. Public here because selectors, and nothing
/// else in this crate's public API, need it by name.
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// The 4-byte function selector for a canonical signature like
/// `"transfer(address,uint256)"`. The signature must already be canonical —
/// no spaces, no parameter names — because this hashes it verbatim.
pub fn selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

/// One ABI argument. Uints and addresses are static (one word in the head);
/// strings and address arrays are dynamic (an offset in the head, data in
/// the tail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Uint(u128),
    Address(WalletAddress),
    Str(String),
    Addresses(Vec<WalletAddress>),
}

fn uint_word(value: u128) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

fn address_word(address: &WalletAddress) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(&address.to_bytes());
    word
}

fn padded(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let rem = out.len() % 32;
    if rem != 0 {
        out.extend(std::iter::repeat_n(0u8, 32 - rem));
    }
    out
}

/// Encode an argument list with the standard head/tail layout. This is the
/// body of a call (or of constructor arguments) — no selector.
pub fn encode_args(args: &[Arg]) -> Vec<u8> {
    let head_len = args.len() * 32;
    let mut head: Vec<u8> = Vec::with_capacity(head_len);
    let mut tail: Vec<u8> = Vec::new();
    for arg in args {
        match arg {
            Arg::Uint(v) => head.extend_from_slice(&uint_word(*v)),
            Arg::Address(a) => head.extend_from_slice(&address_word(a)),
            Arg::Str(s) => {
                head.extend_from_slice(&uint_word((head_len + tail.len()) as u128));
                tail.extend_from_slice(&uint_word(s.len() as u128));
                tail.extend_from_slice(&padded(s.as_bytes()));
            }
            Arg::Addresses(list) => {
                head.extend_from_slice(&uint_word((head_len + tail.len()) as u128));
                tail.extend_from_slice(&uint_word(list.len() as u128));
                for a in list {
                    tail.extend_from_slice(&address_word(a));
                }
            }
        }
    }
    head.extend(tail);
    head
}

/// Encode a full call: selector + arguments.
pub fn encode_call(signature: &str, args: &[Arg]) -> Vec<u8> {
    let mut out = selector(signature).to_vec();
    out.extend(encode_args(args));
    out
}

// ---------------------------------------------------------------- decoding --

fn output_bytes(output: &str) -> Result<Vec<u8>, ChainError> {
    let hex_part = output.trim().trim_start_matches("0x");
    hex::decode(hex_part).map_err(|_| ChainError::InvalidQuantity)
}

fn word_uint(bytes: &[u8], word: usize) -> Result<u128, ChainError> {
    let start = word * 32;
    let end = start + 32;
    if bytes.len() < end {
        return Err(ChainError::InvalidQuantity);
    }
    let w = &bytes[start..end];
    if w[..16].iter().any(|&b| b != 0) {
        return Err(ChainError::Overflow);
    }
    let mut v = [0u8; 16];
    v.copy_from_slice(&w[16..]);
    Ok(u128::from_be_bytes(v))
}

/// The `word`-th 32-byte word of a return payload, as a uint.
pub fn decode_uint(output: &str, word: usize) -> Result<u128, ChainError> {
    decode_words(output).and_then(|b| word_uint(&b, word))
}

fn decode_words(output: &str) -> Result<Vec<u8>, ChainError> {
    let bytes = output_bytes(output)?;
    if bytes.is_empty() || bytes.len() % 32 != 0 {
        return Err(ChainError::InvalidQuantity);
    }
    Ok(bytes)
}

/// A returned `address` (word 0), as lowercase `0x…` hex.
pub fn decode_address(output: &str) -> Result<String, ChainError> {
    let bytes = decode_words(output)?;
    if bytes.len() < 32 {
        return Err(ChainError::InvalidQuantity);
    }
    Ok(format!("0x{}", hex::encode(&bytes[12..32])))
}

/// A returned dynamic `string`: offset word, length word, UTF-8 bytes.
/// Invalid UTF-8 decodes lossily — a token with a garbage symbol should show
/// garbage, not fail the whole listing.
pub fn decode_string(output: &str) -> Result<String, ChainError> {
    let bytes = decode_words(output)?;
    let offset = word_uint(&bytes, 0)? as usize;
    if offset + 32 > bytes.len() {
        return Err(ChainError::InvalidQuantity);
    }
    let len = word_uint(&bytes, offset / 32)? as usize;
    let start = offset + 32;
    if start + len > bytes.len() {
        return Err(ChainError::InvalidQuantity);
    }
    Ok(String::from_utf8_lossy(&bytes[start..start + len]).into_owned())
}

/// A returned `uint256[]`: offset word, length word, items. This is the shape
/// of the router's `getAmountsOut`.
pub fn decode_uint_array(output: &str) -> Result<Vec<u128>, ChainError> {
    let bytes = decode_words(output)?;
    let offset = word_uint(&bytes, 0)? as usize;
    if offset % 32 != 0 || offset + 32 > bytes.len() {
        return Err(ChainError::InvalidQuantity);
    }
    let len = word_uint(&bytes, offset / 32)? as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(word_uint(&bytes, offset / 32 + 1 + i)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(tail: &str) -> WalletAddress {
        WalletAddress::new(&format!("0x{:0>40}", tail)).unwrap()
    }

    #[test]
    fn selectors_match_the_chain_registry_of_record() {
        // Every one of these is a published, widely-deployed selector; the
        // test pins our keccak + canonicalisation against external truth.
        for (signature, expected) in [
            ("transfer(address,uint256)", "a9059cbb"),
            ("balanceOf(address)", "70a08231"),
            ("approve(address,uint256)", "095ea7b3"),
            ("allowance(address,address)", "dd62ed3e"),
            ("decimals()", "313ce567"),
            ("symbol()", "95d89b41"),
            ("name()", "06fdde03"),
            ("totalSupply()", "18160ddd"),
            ("owner()", "8da5cb5b"),
            ("greet()", "cfae3217"),
            ("setGreeting(string)", "a4136862"),
            ("deposit()", "d0e30db0"),
            ("withdraw(uint256)", "2e1a7d4d"),
            ("getAmountsOut(uint256,address[])", "d06ca61f"),
            (
                "swapExactETHForTokens(uint256,address[],address,uint256)",
                "7ff36ab5",
            ),
            (
                "swapExactTokensForETH(uint256,uint256,address[],address,uint256)",
                "18cbafe5",
            ),
            (
                "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
                "38ed1739",
            ),
        ] {
            assert_eq!(hex::encode(selector(signature)), expected, "{signature}");
        }
    }

    #[test]
    fn a_static_call_matches_the_hand_rolled_encoder_in_chain_rs() {
        let to = addr("11");
        let ours = encode_call(
            "transfer(address,uint256)",
            &[Arg::Address(to.clone()), Arg::Uint(500)],
        );
        assert_eq!(ours, crate::chain::erc20_transfer_data(&to, 500));
        let ours = encode_call("balanceOf(address)", &[Arg::Address(to.clone())]);
        assert_eq!(ours, crate::chain::erc20_balance_of_data(&to));
    }

    #[test]
    fn a_dynamic_array_takes_the_head_tail_layout() {
        let a = addr("aa");
        let b = addr("bb");
        let data = encode_call(
            "getAmountsOut(uint256,address[])",
            &[Arg::Uint(1000), Arg::Addresses(vec![a, b])],
        );
        let hex = hex::encode(&data);
        assert_eq!(&hex[..8], "d06ca61f");
        // word 0: amount; word 1: offset 0x40; word 2: length 2; words 3-4: addresses
        assert_eq!(&hex[8..72], &format!("{:064x}", 1000));
        assert_eq!(&hex[72..136], &format!("{:064x}", 0x40));
        assert_eq!(&hex[136..200], &format!("{:064x}", 2));
        assert!(hex[200..264].ends_with("aa"));
        assert!(hex[264..].ends_with("bb"));
    }

    #[test]
    fn a_string_argument_is_length_prefixed_and_padded() {
        let data = encode_call("setGreeting(string)", &[Arg::Str("hello".into())]);
        let hex = hex::encode(&data);
        assert_eq!(&hex[..8], "a4136862");
        assert_eq!(&hex[8..72], &format!("{:064x}", 0x20)); // offset
        assert_eq!(&hex[72..136], &format!("{:064x}", 5)); // length
        assert_eq!(&hex[136..146], hex::encode("hello")); // bytes
        assert_eq!(data.len(), 4 + 32 * 3); // padded to a full word
    }

    #[test]
    fn a_swap_head_reserves_one_word_per_argument() {
        // 4 args, array is arg 2 → its data begins after the 4-word head.
        let data = encode_call(
            "swapExactETHForTokens(uint256,address[],address,uint256)",
            &[
                Arg::Uint(1),
                Arg::Addresses(vec![addr("aa"), addr("bb")]),
                Arg::Address(addr("cc")),
                Arg::Uint(9),
            ],
        );
        let hex = hex::encode(&data);
        assert_eq!(&hex[72..136], &format!("{:064x}", 0x80)); // offset = 4 words
        assert_eq!(data.len(), 4 + 32 * (4 + 1 + 2));
    }

    #[test]
    fn mixed_dynamics_get_sequential_tail_offsets() {
        // (string, string, uint, uint) — the ERC-20 constructor shape.
        let args = encode_args(&[
            Arg::Str("Token".into()),
            Arg::Str("TKN".into()),
            Arg::Uint(18),
            Arg::Uint(1_000_000),
        ]);
        let hex = hex::encode(&args);
        assert_eq!(&hex[..64], &format!("{:064x}", 0x80)); // first tail after 4-word head
        assert_eq!(&hex[64..128], &format!("{:064x}", 0x80 + 0x40)); // after len+data of first
        assert_eq!(&hex[128..192], &format!("{:064x}", 18));
        assert_eq!(&hex[192..256], &format!("{:064x}", 1_000_000));
    }

    #[test]
    fn string_returns_round_trip() {
        let payload = format!(
            "0x{}",
            hex::encode(encode_args(&[Arg::Str("VVS Finance".into())]))
        );
        assert_eq!(decode_string(&payload).unwrap(), "VVS Finance");
        // Empty string is valid.
        let payload = format!("0x{}", hex::encode(encode_args(&[Arg::Str(String::new())])));
        assert_eq!(decode_string(&payload).unwrap(), "");
    }

    #[test]
    fn uint_array_returns_round_trip() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&uint_word(0x20));
        bytes.extend_from_slice(&uint_word(2));
        bytes.extend_from_slice(&uint_word(1_000));
        bytes.extend_from_slice(&uint_word(987_654));
        let payload = format!("0x{}", hex::encode(bytes));
        assert_eq!(decode_uint_array(&payload).unwrap(), vec![1_000, 987_654]);
    }

    #[test]
    fn address_returns_decode_lowercased() {
        let payload = format!("0x{}", hex::encode(address_word(&addr("AbCd"))));
        assert_eq!(
            decode_address(&payload).unwrap(),
            format!("0x{:0>40}", "abcd")
        );
    }

    #[test]
    fn malformed_outputs_error_rather_than_panic() {
        assert!(decode_string("0x1234").is_err()); // not word-aligned
        assert!(decode_uint_array("0x").is_err()); // empty
        assert!(decode_uint("0xzz", 0).is_err()); // not hex
                                                  // Offset pointing past the payload.
        let lie = format!("0x{}", hex::encode(uint_word(0x200)));
        assert!(decode_string(&lie).is_err());
        assert!(decode_uint_array(&lie).is_err());
    }

    #[test]
    fn oversized_uints_report_overflow_not_truncation() {
        let mut word = [0xffu8; 32];
        word[31] = 0x01;
        let payload = format!("0x{}", hex::encode(word));
        assert!(matches!(
            decode_uint(&payload, 0),
            Err(ChainError::Overflow)
        ));
    }
}
