//! `msgHash` — the per-event digest the server stores and that can be published
//! on-chain.
//!
//! Always `hex(SHA-256(utf8(S)))`, lowercase, 64 characters. Plain SHA-256, not
//! Keccak — the address machinery uses Keccak, this does not, and the server
//! regex (`^[a-f0-9]{64}$`) rejects uppercase, so the lowercase output of
//! `hex::encode` is load-bearing.
//!
//! What differs per event is `S`, and the one that matters is the encrypted
//! case: it hashes the **base64 ciphertext**, never the plaintext. That is a
//! security requirement, not a formatting choice — see [`msg_hash_encrypted`].

use sha2::{Digest, Sha256};

use crate::ids::{MessageId, WalletAddress};

/// `hex(SHA-256(data))`, lowercase.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `msgHash` for an **encrypted** message: SHA-256 over the base64 ciphertext
/// string, as ASCII text, `=` padding included.
///
/// Hashing the plaintext instead would let anyone with the stored hash confirm
/// guessed contents by dictionary attack — for short messages ("yes", "on my
/// way", an address, an amount) that defeats the encryption entirely. And
/// because `msgHash` may end up in on-chain calldata, the leak would be
/// permanent and public.
///
/// Note this hashes the base64 *characters*, not the decoded ciphertext bytes.
pub fn msg_hash_encrypted(ciphertext_base64: &str) -> String {
    sha256_hex(ciphertext_base64.as_bytes())
}

/// `msgHash` for a **plaintext** message: SHA-256 over the trimmed content.
///
/// The trim is mandatory. The server applies `.trim()` to `content` before
/// storing it, so hashing the untrimmed string would persist a hash that does
/// not match the persisted content — and any later integrity check, including
/// an on-chain one, would fail forever.
pub fn msg_hash_plaintext(content: &str) -> String {
    sha256_hex(content.trim().as_bytes())
}

/// `msgHash` for an **edit**.
///
/// An edit is a new event, re-encrypted under the current epoch with a fresh
/// IV, so the rule is identical to a new message: hash whatever ends up in
/// `content`, ciphertext or trimmed plaintext — never the original's hash.
pub fn msg_hash_edit(new_content: &str, is_encrypted: bool) -> String {
    if is_encrypted {
        msg_hash_encrypted(new_content)
    } else {
        msg_hash_plaintext(new_content)
    }
}

/// `msgHash` for a **delete**: the empty string.
///
/// The server force-sets `msgHash = ""`, `content = ""`, `iv = null`,
/// `hmac = null`. Nothing is hashed — a deleted message must leave no digest
/// behind that could confirm what it said.
pub fn msg_hash_delete() -> &'static str {
    ""
}

/// Whether an emoticon event added or removed a reaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmoticonAction {
    /// A reaction was added.
    Add,
    /// A reaction was removed.
    Remove,
}

impl EmoticonAction {
    /// The literal token used in the hash input.
    fn as_str(self) -> &'static str {
        match self {
            EmoticonAction::Add => "add",
            EmoticonAction::Remove => "remove",
        }
    }
}

/// `msgHash` for an emoticon event.
///
/// **Server-computed and effectively read-only for a client.** The input embeds
/// the server's `Date.now()`, which no client can reproduce, so this function
/// cannot be used to *validate* a value the server returned — store what you
/// are given. It exists so a Rust server implementation has one definition of
/// the layout, and so the format is documented in code rather than only in
/// prose.
///
/// ```text
/// eventData = "{messageId}:{emoticonCode}:{add|remove}:{senderWalletAddress}:{timestampMs}"
/// ```
///
/// Colons are the only separators and the emoticon code (Unicode, 1..64 chars)
/// is inserted verbatim, so the encoding is ambiguous if a code ever contains a
/// colon. That is upstream's design; it is reproduced, not fixed, because
/// "fixing" it would change every stored hash.
pub fn msg_hash_emoticon(
    message_id: &MessageId,
    emoticon_code: &str,
    action: EmoticonAction,
    sender: &WalletAddress,
    timestamp_ms: i64,
) -> String {
    let event_data = format!(
        "{message_id}:{emoticon_code}:{}:{sender}:{timestamp_ms}",
        action.as_str()
    );
    sha256_hex(event_data.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_hashes_match_the_canonical_vectors() {
        for (ciphertext, expected) in [
            (
                "3nP4XMnquk7mpaDFxNxnZA==",
                "62a40b0ecaa574014eaf4c19c518bb596ed502b4c5e55c01e3e647e7b3f9e3a9",
            ),
            (
                "jLykKspwGTDA6abyS7HrIsSbcL6kRO4RixQVgE+VlJk=",
                "cfb37d96881cc09c28e71882fd1db8597cb842997edd0e12ca0561ddef2b5994",
            ),
            (
                "AeHMd1L87BW8NOlkHslfgN7D7U3yQPnhvrm9X20aeh8=",
                "5b75ba267ea223ed430e57b0aea603fbb1439c8b6123614ed4990088b4aea7ad",
            ),
        ] {
            assert_eq!(msg_hash_encrypted(ciphertext), expected);
        }
    }

    #[test]
    fn base64_padding_is_part_of_the_hash_input() {
        assert_ne!(
            msg_hash_encrypted("3nP4XMnquk7mpaDFxNxnZA=="),
            msg_hash_encrypted("3nP4XMnquk7mpaDFxNxnZA")
        );
    }

    #[test]
    fn encrypted_hash_is_over_the_ciphertext_not_the_plaintext() {
        assert_ne!(
            msg_hash_encrypted("3nP4XMnquk7mpaDFxNxnZA=="),
            msg_hash_plaintext("attack at dawn")
        );
    }

    #[test]
    fn plaintext_hash_matches_plain_sha256_and_is_lowercase() {
        // SHA-256("abc") — the FIPS 180-2 example.
        let got = msg_hash_plaintext("abc");
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(got, got.to_lowercase());
        assert_eq!(got.len(), 64);
    }

    #[test]
    fn plaintext_hash_trims_first() {
        assert_eq!(
            msg_hash_plaintext("  hello \n"),
            msg_hash_plaintext("hello")
        );
        assert_ne!(
            msg_hash_plaintext("  hello \n"),
            sha256_hex(b"  hello \n"),
            "hashing the untrimmed string would not match what the server stores"
        );
    }

    #[test]
    fn edit_dispatches_on_encryption() {
        assert_eq!(msg_hash_edit("abc", false), msg_hash_plaintext("abc"));
        assert_eq!(
            msg_hash_edit("3nP4XMnquk7mpaDFxNxnZA==", true),
            msg_hash_encrypted("3nP4XMnquk7mpaDFxNxnZA==")
        );
    }

    #[test]
    fn delete_is_the_empty_string_not_a_hash_of_nothing() {
        assert_eq!(msg_hash_delete(), "");
        assert_ne!(msg_hash_delete(), sha256_hex(b""));
    }

    #[test]
    fn emoticon_layout_is_colon_separated() {
        let id = MessageId::new("msg_1749652739650_304e0eaf").unwrap();
        let sender = WalletAddress::new("0xF39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap();
        let got = msg_hash_emoticon(&id, "🍓", EmoticonAction::Add, &sender, 1_749_652_739_650);

        // The address is lowercased by the type, matching the server's JWT
        // payload, and the layout is reproduced exactly.
        let expected = sha256_hex(
            "msg_1749652739650_304e0eaf:🍓:add:0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266:1749652739650"
                .as_bytes(),
        );
        assert_eq!(got, expected);

        // add and remove must not collide.
        assert_ne!(
            got,
            msg_hash_emoticon(
                &id,
                "🍓",
                EmoticonAction::Remove,
                &sender,
                1_749_652_739_650
            )
        );
    }
}
