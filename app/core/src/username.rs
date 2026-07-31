//! Deterministic display names, derived from a wallet address.
//!
//! A wallet has no name, and 42 characters of hex is not one. So an account
//! that never chose a username is given `AdjectiveNoun####` — memorable,
//! collision-resistant enough (152 × 156 × 10 000 ≈ 237 million), and above all
//! **stable**: the same address produces the same name on every device, in
//! every client, forever. That is what makes it safe to fill in automatically
//! rather than asking someone to invent a name before they have even seen their
//! wallet.
//!
//! # The algorithm (PROTOCOL.md §10)
//!
//! ```text
//! hash    = keccak256(utf8(lowercase(address)))   // the "0x" is included
//! adj     = uint16(hash[0..2])  % ADJECTIVES.len()
//! noun    = uint16(hash[2..4])  % NOUNS.len()
//! suffix  = uint16(hash[4..6])  % 10000           // zero-padded to 4 digits
//! name    = ADJECTIVES[adj] + NOUNS[noun] + suffix
//! ```
//!
//! Two properties are load-bearing and both are pinned by the tests below:
//!
//! - It hashes the **address**, never the mnemonic. A wallet reached by
//!   recovery phrase and the same wallet reached by private key (or by
//!   MetaMask) must arrive at one name; deriving from the credential would give
//!   the same account a different name depending on how you signed in.
//! - The address is lowercased first, so checksummed and lowercase spellings
//!   agree. [`WalletAddress`] already normalises, which makes that automatic
//!   here — but the vectors cover it anyway, because the invariant belongs to
//!   the algorithm rather than to the type that happens to enforce it.
//!
//! The word lists are the reference client's (`client/src/lib/usernameWords.ts`)
//! transcribed verbatim, in order. **Order is protocol.** Inserting a word
//! anywhere but the end renames every account whose index falls after it, which
//! is not a cosmetic change — it is a different name for the same wallet on two
//! clients that disagree.

use sha3::{Digest, Keccak256};

use crate::ids::WalletAddress;

/// 152 adjectives, in the reference client's order.
pub const ADJECTIVES: [&str; 152] = [
    "Epic",
    "Cyber",
    "Neon",
    "Mystic",
    "Cosmic",
    "Shadow",
    "Phoenix",
    "Quantum",
    "Stellar",
    "Thunder",
    "Blaze",
    "Frost",
    "Storm",
    "Vortex",
    "Turbo",
    "Alpha",
    "Nova",
    "Prism",
    "Apex",
    "Titan",
    "Zephyr",
    "Crimson",
    "Onyx",
    "Jade",
    "Atomic",
    "Binary",
    "Chrome",
    "Digital",
    "Plasma",
    "Sonic",
    "Vector",
    "Hyper",
    "Nano",
    "Omega",
    "Delta",
    "Gamma",
    "Sigma",
    "Zero",
    "Neo",
    "Meta",
    "Pixel",
    "Glitch",
    "Matrix",
    "Neural",
    "Synth",
    "Techno",
    "Vertex",
    "Photon",
    "Solar",
    "Lunar",
    "Arctic",
    "Ember",
    "Aqua",
    "Terra",
    "Volt",
    "Aero",
    "Pyro",
    "Cryo",
    "Inferno",
    "Aurora",
    "Eclipse",
    "Nebula",
    "Typhoon",
    "Tsunami",
    "Magma",
    "Quake",
    "Tidal",
    "Volcanic",
    "Blizzard",
    "Tempest",
    "Savage",
    "Fierce",
    "Swift",
    "Silent",
    "Stealth",
    "Rapid",
    "Prime",
    "Ultra",
    "Mega",
    "Giga",
    "Super",
    "Elite",
    "Royal",
    "Noble",
    "Supreme",
    "Grand",
    "Majestic",
    "Imperial",
    "Dominant",
    "Mighty",
    "Valor",
    "Glory",
    "Arcane",
    "Astral",
    "Ethereal",
    "Void",
    "Dark",
    "Light",
    "Crystal",
    "Golden",
    "Silver",
    "Iron",
    "Steel",
    "Diamond",
    "Ruby",
    "Sapphire",
    "Obsidian",
    "Mythic",
    "Legendary",
    "Ancient",
    "Primal",
    "Divine",
    "Sacred",
    "Cursed",
    "Blessed",
    "Enchanted",
    "Rogue",
    "Rebel",
    "Wild",
    "Feral",
    "Brave",
    "Bold",
    "Daring",
    "Radiant",
    "Blazing",
    "Frozen",
    "Wicked",
    "Chaos",
    "Fury",
    "Venomous",
    "Lethal",
    "Fatal",
    "Deadly",
    "Ruthless",
    "Fearless",
    "Relentless",
    "Azure",
    "Scarlet",
    "Violet",
    "Indigo",
    "Cobalt",
    "Emerald",
    "Amber",
    "Ivory",
    "Ebony",
    "Platinum",
    "Copper",
    "Bronze",
    "Nether",
    "Astro",
    "Galactic",
    "Celestial",
];

/// 156 nouns, in the reference client's order.
pub const NOUNS: [&str; 156] = [
    "Wolf",
    "Hawk",
    "Dragon",
    "Ninja",
    "Samurai",
    "Knight",
    "Ranger",
    "Hunter",
    "Warrior",
    "Guardian",
    "Phantom",
    "Viper",
    "Falcon",
    "Tiger",
    "Panther",
    "Raven",
    "Eagle",
    "Lion",
    "Shark",
    "Fox",
    "Lynx",
    "Cobra",
    "Bear",
    "Leopard",
    "Phoenix",
    "Griffin",
    "Hydra",
    "Kraken",
    "Sphinx",
    "Chimera",
    "Wyvern",
    "Raptor",
    "Basilisk",
    "Cerberus",
    "Leviathan",
    "Fenrir",
    "Pegasus",
    "Minotaur",
    "Gargoyle",
    "Wyrm",
    "Drake",
    "Behemoth",
    "Cyclops",
    "Titan",
    "Scorpion",
    "Mantis",
    "Spider",
    "Jaguar",
    "Puma",
    "Orca",
    "Crow",
    "Owl",
    "Serpent",
    "Python",
    "Mustang",
    "Stallion",
    "Rhino",
    "Gorilla",
    "Wolverine",
    "Badger",
    "Condor",
    "Vulture",
    "Barracuda",
    "Piranha",
    "Mamba",
    "Hornet",
    "Wasp",
    "Beetle",
    "Paladin",
    "Ronin",
    "Shogun",
    "Viking",
    "Spartan",
    "Gladiator",
    "Crusader",
    "Assassin",
    "Sentinel",
    "Warden",
    "Champion",
    "Commander",
    "Captain",
    "Admiral",
    "General",
    "Marshal",
    "Berserker",
    "Centurion",
    "Legionnaire",
    "Templar",
    "Mercenary",
    "Pirate",
    "Bandit",
    "Outlaw",
    "Wizard",
    "Sorcerer",
    "Mage",
    "Warlock",
    "Druid",
    "Shaman",
    "Oracle",
    "Prophet",
    "Reaper",
    "Specter",
    "Wraith",
    "Ghost",
    "Spirit",
    "Demon",
    "Angel",
    "Golem",
    "Necromancer",
    "Alchemist",
    "Enchanter",
    "Summoner",
    "Invoker",
    "Seraph",
    "Valkyrie",
    "Djinn",
    "Blade",
    "Sword",
    "Dagger",
    "Arrow",
    "Bolt",
    "Comet",
    "Meteor",
    "Pulsar",
    "Quasar",
    "Star",
    "Moon",
    "Sun",
    "Flame",
    "Striker",
    "Breaker",
    "Slayer",
    "Hammer",
    "Axe",
    "Spear",
    "Scythe",
    "Trident",
    "Shield",
    "Crown",
    "Throne",
    "Hacker",
    "Cipher",
    "Virus",
    "Coder",
    "Sniper",
    "Gunner",
    "Pilot",
    "Driver",
    "Racer",
    "Runner",
    "Bomber",
    "Tank",
    "Drone",
    "Mech",
    "Android",
    "Cyborg",
];

/// The three picks the digest makes: adjective, noun, and a 4-digit suffix.
fn pick(digest: &[u8]) -> (&'static str, &'static str, u16) {
    let at = |i: usize| u16::from_be_bytes([digest[i], digest[i + 1]]) as usize;
    (
        ADJECTIVES[at(0) % ADJECTIVES.len()],
        NOUNS[at(2) % NOUNS.len()],
        (at(4) % 10_000) as u16,
    )
}

/// The name this address is known by when nobody has chosen one.
///
/// Always 3–100 characters of ASCII letters and digits, so it passes the
/// server's `username` validation without any further massaging.
pub fn deterministic_username(address: &WalletAddress) -> String {
    let digest = Keccak256::digest(address.as_str().as_bytes());
    let (adjective, noun, suffix) = pick(&digest);
    format!("{adjective}{noun}{suffix:04}")
}

/// A room name from arbitrary entropy — the same two word lists, spaced, for
/// something a human reads as a title rather than as a handle.
///
/// Unlike [`deterministic_username`] this is *meant* to be fed randomness: two
/// people creating a room in the same second should not land on the same name.
/// It stays a pure function of its input so it can be tested without a source
/// of randomness anywhere near it.
pub fn room_name_from_entropy(entropy: &[u8]) -> String {
    let digest = Keccak256::digest(entropy);
    let (adjective, noun, suffix) = pick(&digest);
    format!("{adjective} {noun} {suffix:04}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by the reference implementation (`ethers.keccak256` over the
    /// lowercased address, against the same word lists) and cross-checked
    /// against PROTOCOL.md §10 for the first two. Any change to the algorithm
    /// or to the *order* of the word lists breaks these, which is the point.
    const VECTORS: [(&str, &str); 4] = [
        // PROTOCOL.md §10 vector 1 — `test…junk` at index 0.
        (
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
            "OmegaMustang0198",
        ),
        // PROTOCOL.md §10 vector 2 — the same phrase at index 1.
        (
            "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
            "AmberLion9030",
        ),
        // The all-`abandon` phrase at index 0, and secp256k1 scalar 1 — the two
        // wallets the login tests already use.
        (
            "0x9858effd232b4033e47d90003d41ec34ecaeda94",
            "AmberEnchanter2784",
        ),
        (
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf",
            "AmberRunner7074",
        ),
    ];

    fn addr(s: &str) -> WalletAddress {
        WalletAddress::new(s).unwrap()
    }

    #[test]
    fn the_word_lists_are_the_reference_clients_lists() {
        // The lengths are part of the wire contract: they are the modulus. A
        // list that grew by one word renames roughly every account.
        assert_eq!(ADJECTIVES.len(), 152);
        assert_eq!(NOUNS.len(), 156);
    }

    #[test]
    fn names_match_the_reference_implementation() {
        for (address, expected) in VECTORS {
            assert_eq!(
                deterministic_username(&addr(address)),
                expected,
                "diverged from the reference for {address}"
            );
        }
    }

    #[test]
    fn casing_of_the_address_cannot_change_the_name() {
        // The invariant PROTOCOL.md §10 calls out: a checksummed address (what
        // MetaMask hands you) and a lowercase one (what the API returns) are
        // the same account and must produce the same name.
        let lower = addr("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
        let checksummed = addr("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        assert_eq!(
            deterministic_username(&lower),
            deterministic_username(&checksummed)
        );
    }

    #[test]
    fn a_name_always_passes_the_servers_username_rules() {
        // 3–100 characters, no markup, no control characters — validated
        // server-side. A generated name that the server then rejects would
        // dead-end the one flow this exists to unblock.
        for i in 0u32..512 {
            let name = deterministic_username(&addr(&format!("0x{i:040x}")));
            let len = name.chars().count();
            assert!((3..=100).contains(&len), "{name} is {len} characters");
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric()),
                "{name} is not plain alphanumeric"
            );
            // …and it ends in exactly four digits, so it never collides with a
            // bare dictionary word someone might have chosen by hand.
            assert!(name[name.len() - 4..].chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn different_wallets_get_different_names() {
        let names: std::collections::HashSet<String> = (0u32..1024)
            .map(|i| deterministic_username(&addr(&format!("0x{i:040x}"))))
            .collect();
        // Not "all distinct" — a hash over a 237-million-name space will
        // collide eventually and pretending otherwise would make this flaky.
        // What matters is that the name is a function of the whole address, not
        // of a handful of bits.
        assert!(names.len() > 1000, "only {} distinct names", names.len());
    }

    #[test]
    fn room_names_read_as_titles_and_track_their_entropy() {
        let a = room_name_from_entropy(&[0u8; 16]);
        let b = room_name_from_entropy(&[1u8; 16]);
        assert_ne!(a, b, "the entropy must reach the name");
        // Three words: adjective, noun, digits. The space is what stops it
        // looking like a username.
        let parts: Vec<&str> = a.split(' ').collect();
        assert_eq!(parts.len(), 3, "unexpected shape: {a}");
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].chars().all(|c| c.is_ascii_digit()));
        // Well inside the server's 1–100 character room-name limit.
        assert!(a.chars().count() <= 100);
        // Pure: the same entropy is the same name.
        assert_eq!(a, room_name_from_entropy(&[0u8; 16]));
    }
}
