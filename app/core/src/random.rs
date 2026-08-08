//! Where everything unguessable in this crate comes from.
//!
//! # Why a module for what is, in the end, one function call
//!
//! Not because what came before it was broken. This crate drew entropy from
//! `rand::rngs::OsRng` at four call sites across two modules, and the server
//! drew it from `rand::thread_rng()` at three more. Both of those are CSPRNGs
//! seeded from the operating system; neither was a weakness, and calling this
//! change a security fix would be dishonest.
//!
//! The reason is auditability. "Is every secret in this system drawn from the
//! OS?" should be a question one file answers, not one that means grepping for
//! several spellings of the same idea and then reasoning, site by site, about
//! which of them are on a security path and whether each of them fails
//! usefully. One place to check, and one place that cannot be quietly weakened
//! later — because with `rand` gone from this crate's dependency list there is
//! nowhere else for a weakening to hide. [`crate::wallet`] has claimed exactly
//! this in a doc comment since it was written; this module is what makes the
//! claim literally true rather than approximately true.
//!
//! # No fallback, ever
//!
//! Every function here returns [`CryptoError::Randomness`] when the OS refuses
//! to produce bytes. There is no seeded backup generator, no clock-derived
//! stopgap, no zero-filled buffer returned with a logged warning. A room key
//! that is secretly `[0u8; 32]` is far worse than a room that could not be
//! created: the failure is invisible to everyone, it is permanent in every
//! message already sealed under that key, and no amount of later care can
//! detect it from the outside. A caller that cannot propagate the error should
//! refuse to perform the operation.
//!
//! Note what the signatures make impossible. [`bytes`] and [`secret_key`] hand
//! back a value only on success, so there is no failure path on which a caller
//! ends up holding something it believes is random. [`fill`] exists for the
//! variable-length case and is the one function where that care falls to the
//! caller, which is why its contract on failure is spelled out below.
//!
//! # Which generator, and why it works in a browser
//!
//! `getrandom` directly, rather than through `rand`'s `OsRng`, which is a
//! wrapper over `getrandom` anyway. One dependency on the path between the
//! operating system and a private key instead of two, for the same reason
//! [`crate::wallet`] implements BIP-32 by hand rather than taking a crate for
//! it. `OsRng` was not the wrong choice; it was simply a layer that buys
//! nothing here. It buys something worse than nothing on the failure path:
//! `rand`'s `RngCore::fill_bytes` has no way to signal that the OS refused, so
//! `OsRng` turns a refusal into a panic where `getrandom::getrandom` returns an
//! `Err` this module can propagate. (That is a property of the `RngCore` trait
//! contract, not of any one `rand` version, so it does not go stale under a
//! future `rand`/`getrandom` bump.)
//!
//! This crate compiles for `wasm32-unknown-unknown` as well as natively, and a
//! browser has no `/dev/urandom`. `Cargo.toml` turns on `getrandom`'s `js`
//! feature for that target, which routes these calls at
//! `crypto.getRandomValues` — the browser's own CSPRNG, and the same source the
//! reference TypeScript client drew from. Nothing in this module is
//! target-specific; the substitution happens underneath it, which is what makes
//! "one place to audit" true on both targets at once.
//!
//! This module doc is the single home of that argument. Everything else that
//! draws from here — `wallet::generate_mnemonic`, the server's
//! `auth::random_hex_32`, the two `Cargo.toml` notes — points back to it rather
//! than restating it, so there is one place to change if the reasoning does.

use k256::SecretKey;

use crate::crypto::CryptoError;

/// How many times [`secret_key`] will redraw before giving up.
///
/// Not a rejection budget. A 32-byte draw lands outside secp256k1's `[1, n)` —
/// zero, or at or above the group order — with probability below 2⁻¹²⁷, so a
/// single retry would already be more than the mathematics asks for. The limit
/// is there for the other failure mode: an entropy source that has started
/// returning a constant the curve rejects would otherwise spin in this loop
/// forever, and a hang is a worse way to fail than an error.
const SECRET_KEY_ATTEMPTS: usize = 8;

/// Fill `out` with bytes from the OS (or browser) CSPRNG.
///
/// **On failure the contents of `out` are unspecified.** `getrandom` makes no
/// promise about how much of the buffer it touched before the error, so a
/// caller that ignores the `Result` is not left with zeros — it is left with
/// something that may be *partly* random, which is the most dangerous shape a
/// failed draw can take. Prefer [`bytes`], which cannot be misused that way,
/// and use this only when the length is not known at compile time.
pub fn fill(out: &mut [u8]) -> Result<(), CryptoError> {
    // The single test seam in this module. `#[cfg(test)]` on the statement
    // means the branch is not merely unreachable in a release build, it is
    // absent from it — there is no flag a running process could flip. It earns
    // its place because the alternative is shipping an error path that has
    // never been executed, and the error path is the whole point of the module.
    #[cfg(test)]
    if FAILING.with(std::cell::Cell::get) {
        return Err(CryptoError::Randomness);
    }
    getrandom::getrandom(out).map_err(|_| CryptoError::Randomness)
}

/// `N` bytes from the OS (or browser) CSPRNG.
///
/// The array exists only if the draw succeeded, which is the property that
/// makes this the function to reach for: IVs, room keys, salts and tokens are
/// all fixed-size, and none of their call sites should have to think about what
/// a half-filled buffer means.
pub fn bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut out = [0u8; N];
    fill(&mut out)?;
    Ok(out)
}

/// A uniformly random secp256k1 secret key.
///
/// Rejection sampling: draw 32 bytes, keep them if they are a scalar in
/// `[1, n)`, redraw otherwise. That is the same loop `k256::SecretKey::random`
/// runs and it produces the same distribution — taking the raw draw modulo `n`
/// instead would be the tempting one-liner and would bias the low end of the
/// range, which is exactly the kind of quiet weakening this module exists to
/// prevent.
///
/// The reason not to simply call `SecretKey::random(&mut OsRng)` is the failure
/// path. It takes an `RngCore`, whose `fill_bytes` has no way to report that
/// the OS refused, so `OsRng` panics instead — in a browser, a blank tab. Here
/// the caller gets a [`CryptoError::Randomness`] it can put in front of a
/// person, and every entropy draw in the crate reports failure the same way.
pub fn secret_key() -> Result<SecretKey, CryptoError> {
    for _ in 0..SECRET_KEY_ATTEMPTS {
        let candidate = bytes::<32>()?;
        if let Ok(secret) = SecretKey::from_slice(&candidate) {
            return Ok(secret);
        }
    }
    Err(CryptoError::Randomness)
}

/// 32 CSPRNG bytes as 64 lowercase hex characters.
///
/// The shape every unguessable *string* in the system is built from: the
/// server's login challenge nonce, its SSE ticket, its invite and webhook
/// tokens, and a per-account encryption salt. Those live in the server crate
/// and each maps this into its own error type, but the draw-and-encode is here
/// so the one choke point covers strings as well as raw bytes — a hex token is
/// not a second, hand-rolled entropy path, it is this function with a prefix.
pub fn hex_32() -> Result<String, CryptoError> {
    Ok(hex::encode(bytes::<32>()?))
}

#[cfg(test)]
std::thread_local! {
    /// Set by [`FailureGuard`]; read by the one `#[cfg(test)]` branch in
    /// [`fill`]. Thread-local so that the cargo test harness running tests in
    /// parallel cannot let one test's forced failure reach another's draw.
    static FAILING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes every entropy draw on the current thread fail until it is dropped.
///
/// Test-only, and deliberately an RAII guard rather than a pair of
/// set/clear functions: a test that panics mid-way must not leave the flag
/// standing for whatever the harness schedules onto this thread next.
///
/// It carries the *previous* value of the flag rather than assuming the flag
/// was clear, so nesting composes: an inner guard's `Drop` re-arms the outer
/// scope instead of disarming it. That is not idle generality — the flag is a
/// thread-local shared by every test on the thread, and a guard that clobbered
/// it on drop would make the seam's own behaviour depend on scoping accidents.
///
/// This is the whole of the seam. Production code has no way to reach it, no
/// injectable RNG parameter, and no trait object between a call site and the
/// OS — an entropy source that can be substituted at run time is precisely the
/// thing whose absence the rest of this module is arguing for.
#[cfg(test)]
pub(crate) struct FailureGuard {
    previous: bool,
}

#[cfg(test)]
impl FailureGuard {
    pub(crate) fn new() -> Self {
        let previous = FAILING.with(|f| f.replace(true));
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for FailureGuard {
    fn drop(&mut self) {
        FAILING.with(|f| f.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sixteen draws of 32 bytes. Every one is a fresh 256-bit value, so any
    /// repeat, and any all-zero result, is a real defect and not bad luck — the
    /// probability of either happening by chance is far below the probability
    /// of the machine running the test being struck by lightning mid-assert.
    ///
    /// What this does *not* do is test the quality of the generator. Nothing
    /// runnable in a unit test can: a counter starting at a random offset would
    /// pass every assertion below. These are wiring checks — that the bytes
    /// reach the caller, that they are not a constant, and that nobody has
    /// quietly replaced the body with a zero fill.
    #[test]
    fn successive_draws_differ_and_are_never_zero() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..16 {
            let drawn = bytes::<32>().expect("the OS CSPRNG should answer");
            assert_ne!(drawn, [0u8; 32], "a zero-filled draw is never acceptable");
            assert!(seen.insert(drawn), "two draws of 32 bytes collided");
        }
    }

    /// The same, for [`fill`], and with a buffer that starts non-zero so the
    /// test would notice a body that returned `Ok` without writing anything.
    #[test]
    fn fill_overwrites_the_buffer_it_is_given() {
        let mut buf = [0xAAu8; 48];
        fill(&mut buf).expect("the OS CSPRNG should answer");
        assert!(buf.iter().any(|&b| b != 0xAA), "the buffer was not written");
        assert!(buf.iter().any(|&b| b != 0), "the buffer was zero-filled");
    }

    /// Not a distribution test — 512 bytes cannot establish uniformity. It
    /// pins the one degradation a wiring check would otherwise miss: a source
    /// stuck on a single byte value, which `successive_draws_differ` would
    /// happily pass if the stuck value changed between calls.
    #[test]
    fn a_draw_is_not_one_byte_value_repeated() {
        let drawn = bytes::<512>().expect("the OS CSPRNG should answer");
        assert!(
            drawn.iter().any(|&b| b != drawn[0]),
            "512 bytes all equal to {:#04x}",
            drawn[0]
        );
    }

    #[test]
    fn secret_keys_are_valid_scalars_and_differ() {
        let a = secret_key().expect("the OS CSPRNG should answer");
        let b = secret_key().expect("the OS CSPRNG should answer");
        assert_ne!(a.to_bytes(), b.to_bytes());
        // `from_slice` already rejected zero and anything ≥ n; re-importing
        // asserts the value that came out is one a caller can round-trip.
        assert!(SecretKey::from_slice(&a.to_bytes()).is_ok());
    }

    /// The point of the whole module: when the OS will not answer, every entry
    /// point returns an error. None of them returns zeros, a shortened value,
    /// or anything else a caller could mistake for a secret.
    #[test]
    fn every_entry_point_fails_loudly_when_the_os_refuses() {
        let _guard = FailureGuard::new();

        assert_eq!(bytes::<32>().err(), Some(CryptoError::Randomness));
        assert_eq!(secret_key().err(), Some(CryptoError::Randomness));

        let mut buf = [0xAAu8; 16];
        assert_eq!(fill(&mut buf).err(), Some(CryptoError::Randomness));
        // The seam refuses before `getrandom` is reached, so here the buffer
        // is untouched. A *real* failure makes no such promise — see [`fill`] —
        // which is the reason the assertion that matters is the one above it.
        assert_eq!(buf, [0xAAu8; 16]);
    }

    /// The guard is scoped. A test that forces a failure must not poison the
    /// thread for whatever the harness runs next on it.
    #[test]
    fn the_failure_guard_stops_at_its_scope() {
        {
            let _guard = FailureGuard::new();
            assert!(bytes::<8>().is_err());
        }
        assert!(bytes::<8>().is_ok());
    }

    /// Nesting composes: dropping the inner guard restores the *outer* scope's
    /// forced failure rather than clearing it. A guard that clobbered the flag
    /// on drop would silently re-enable entropy in the middle of an outer
    /// failure scope, which is exactly the kind of quiet weakening the seam
    /// exists to make impossible.
    #[test]
    fn nested_guards_restore_the_enclosing_scope() {
        let _outer = FailureGuard::new();
        assert!(bytes::<8>().is_err());
        {
            let _inner = FailureGuard::new();
            assert!(bytes::<8>().is_err());
        }
        // Inner dropped; the outer failure must still stand.
        assert!(bytes::<8>().is_err());
    }
}
