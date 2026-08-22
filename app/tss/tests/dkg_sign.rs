//! End-to-end m-of-n: DKG → seal/unseal the wallet → threshold-sign with two
//! *different* 2-of-3 subsets → recover the wallet address from both.
//!
//! This runs the real DKLs23 protocol (sl-dkls23) — OT-based, so the whole
//! file finishes in seconds. It is the proof behind PROTOCOL.md's MPC
//! section: a ceremony signature is an ordinary Ethereum signature.

use pocketskynet_core::{eip191, WalletAddress};
use pocketskynet_tss::{dkg, eth, sign, store, TssError};

#[test]
fn two_of_three_dkg_signs_with_any_quorum_and_never_below_it() {
    let (t, n) = (2u16, 3u16);

    // 1. DKG.
    let mut eid = [0u8; 32];
    eid[..4].copy_from_slice(b"test");
    let shares = dkg::run_dkg(t, n, eid, |p| println!("dkg phase: {p:?}")).expect("dkg");
    assert_eq!(shares.len(), usize::from(n));

    // Every party must agree on the address.
    let address = eth::eth_address(&shares[0]).unwrap();
    for s in &shares[1..] {
        assert_eq!(eth::eth_address(s).unwrap(), address);
    }

    // 2. Seal into the n user-held share files and open a quorum back
    // through the passphrase — the custody round trip with *real* shares.
    let raw: Vec<String> = shares.iter().map(store::encode_share).collect();
    let files = store::seal_shares(
        &address,
        t,
        n,
        &raw,
        &format!("0x{}", "11".repeat(32)),
        &format!("0x{}", "22".repeat(65)),
        "test passphrase",
    )
    .expect("seal");
    assert_eq!(files.len(), usize::from(n));

    let reload = |quorum: &[usize]| -> Vec<(u16, pocketskynet_tss::Share)> {
        let picked: Vec<store::ShareFile> = quorum.iter().map(|&i| files[i].clone()).collect();
        let opened = store::open_shares(&picked, "test passphrase").expect("open");
        assert_eq!(opened.address, address);
        opened
            .signers
            .into_iter()
            .map(|(i, v)| (i, store::decode_share(&v).expect("a real key share")))
            .collect()
    };

    // 3. Sign an EIP-191 message with the quorum {0, 1} of reloaded shares.
    let message = "hello cronos, threshold edition";
    let prehash = eip191::eip191_digest(message);
    let subset_01 = reload(&[0, 1]);
    let sig =
        sign::sign_prehash(&subset_01, prehash, seed(b"sig1")).expect("sign with parties 0,1");
    assert_signature_is_ordinary(&sig, message, &address);

    // 4. A different quorum — parties {0, 2}, i.e. file 1 lost — signs for
    // the same address. This is the m-of-n property the 2-of-2 reference
    // could not show.
    let subset_02 = reload(&[0, 2]);
    let sig2 =
        sign::sign_prehash(&subset_02, prehash, seed(b"sig2")).expect("sign with parties 0,2");
    assert_signature_is_ordinary(&sig2, message, &address);

    // Ceremony nonces are random, so the two quorums' signatures differ even
    // over the same message — exactly the non-determinism CRYPTO.md §15.2
    // decouples E2EE from.
    assert_ne!(sig.to_hex(), sig2.to_hex());

    // 5. Below the threshold, the signer API refuses.
    let below: Vec<(u16, pocketskynet_tss::Share)> = subset_01[..1].to_vec();
    assert!(matches!(
        sign::sign_prehash(&below, prehash, seed(b"sig3")),
        Err(TssError::InvalidSigners(_))
    ));

    // And a subset that repeats a party is rejected, not silently accepted.
    let doubled = vec![subset_01[0].clone(), subset_01[0].clone()];
    assert!(matches!(
        sign::sign_prehash(&doubled, prehash, seed(b"sig4")),
        Err(TssError::InvalidSigners(_))
    ));
}

/// The indistinguishability requirement, asserted through the same code the
/// server runs on a login: low-s, v ∈ {27,28} wire form, and
/// `eip191::recover_address` — which knows nothing about MPC — must land on
/// the wallet address.
fn assert_signature_is_ordinary(
    sig: &pocketskynet_tss::eth::TssSignature,
    message: &str,
    address: &WalletAddress,
) {
    // Low-s: k256 returns None from normalize_s when s is already low.
    let k_sig = k256::ecdsa::Signature::from_slice(&sig.rs_bytes()).unwrap();
    assert!(k_sig.normalize_s().is_none(), "signature must be low-s");

    let recovered = eip191::recover_address(message, &sig.to_hex()).expect("recover");
    assert_eq!(&recovered, address, "recovered address must be the wallet");
    assert!(eip191::verify_signature(message, &sig.to_hex(), address));
}

fn seed(tag: &[u8; 4]) -> [u8; 32] {
    let mut eid = [1u8; 32];
    eid[..4].copy_from_slice(tag);
    eid
}
