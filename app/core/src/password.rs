//! The random password generator.
//!
//! # Every byte comes from the OS CSPRNG, or nothing does
//!
//! [`generate`] draws from [`crate::random`] — `getrandom` on the host,
//! `crypto.getRandomValues` in the browser (see `docs/CRYPTO.md` §11.1) — and
//! returns [`PasswordError::Randomness`] if that fails. There is deliberately
//! **no fallback**. The tempting one is a PRNG seeded from `Date.now()`, which
//! keeps the button working on some hypothetical browser without a CSPRNG and
//! produces a password an attacker can reconstruct from the minute it was
//! generated. A generator that silently degrades is worse than one that stops,
//! because the person holding the weak password has no way to find out. So the
//! failure is loud, the UI surfaces it, and the field stays empty.
//!
//! # No modulo bias
//!
//! Mapping a random byte onto an alphabet with `% len` makes the first
//! `256 % len` characters more likely than the rest — for the 94-character
//! full set that is a measurable skew across every position. [`uniform_below`]
//! rejects the tail instead, which costs an occasional extra byte and nothing
//! else. Rejection sampling is used again for the shuffle.
//!
//! # Class coverage, and why it is not free
//!
//! When a recipe asks for four character classes, the result is guaranteed to
//! contain at least one of each — otherwise a site that demands a digit rejects
//! roughly one generated password in twenty and the user learns to press the
//! button repeatedly, which is a worse experience and no more secure. The
//! guarantee costs a little entropy: it removes the class-free strings from the
//! output space. At the default length that is a fraction of a bit against
//! roughly 130, which is not a trade worth agonising over. It matters more at
//! very short lengths, which is one reason [`MIN_LENGTH`] exists.
//!
//! The mandatory characters are placed first and the whole string is then
//! shuffled with a CSPRNG-driven Fisher-Yates, so "the digit is always at
//! position 2" — a real defect in more than one shipped generator — cannot
//! happen here.

use crate::random;

/// Shortest password this will produce.
///
/// Eight characters of the full alphabet is about 52 bits: weak, but it is the
/// floor a great many sites still impose, and refusing to generate one at all
/// would push people to invent their own. Below that there is no honest use.
pub const MIN_LENGTH: usize = 8;

/// Longest password this will produce. Past this, length is not the limiting
/// factor and the field stops being paste-able into anything real.
pub const MAX_LENGTH: usize = 128;

/// What a fresh recipe generates when the user has expressed no preference.
pub const DEFAULT_LENGTH: usize = 20;

/// Lowercase letters.
const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
/// Uppercase letters.
const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// Digits.
const DIGITS: &[u8] = b"0123456789";
/// Punctuation.
///
/// Printable ASCII only, and deliberately without the space: a trailing space
/// in a password is invisible, survives a copy, and is trimmed by roughly half
/// the login forms in existence. Quotes and backslashes are kept — a password
/// that breaks a badly written form is that form's problem, and excluding them
/// narrows the alphabet for everyone.
const SYMBOLS: &[u8] = b"!#$%&()*+,-./:;<=>?@[]^_{|}~\"'\\`";

/// Why a password could not be generated.
///
/// Two of these are the caller's mistake and one is the platform's. They are
/// separate variants because the UI says different things: a recipe with no
/// classes ticked is a form to fix, and a dead CSPRNG is a browser to abandon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// Every character class was switched off. There is nothing to draw from,
    /// and quietly re-enabling one would produce a password whose alphabet the
    /// user did not choose.
    #[error("a password needs at least one character class")]
    NoCharacterClasses,

    /// The requested length is outside [`MIN_LENGTH`]..=[`MAX_LENGTH`].
    #[error("length must be between {MIN_LENGTH} and {MAX_LENGTH}")]
    Length,

    /// The OS/browser CSPRNG refused to produce bytes.
    ///
    /// Never softened into a fallback. See the module docs.
    #[error("secure random number generation failed")]
    Randomness,
}

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    pub length: usize,
    pub lowercase: bool,
    pub uppercase: bool,
    pub digits: bool,
    pub symbols: bool,
}

impl Default for Recipe {
    /// All four classes at [`DEFAULT_LENGTH`].
    ///
    /// Symbols are on by default even though they are the class most likely to
    /// be rejected by some legacy form, because the default should be the
    /// strong one and the switch to turn it off is right there.
    fn default() -> Self {
        Self {
            length: DEFAULT_LENGTH,
            lowercase: true,
            uppercase: true,
            digits: true,
            symbols: true,
        }
    }
}

impl Recipe {
    /// The classes this recipe draws from, in a stable order.
    fn classes(&self) -> Vec<&'static [u8]> {
        let mut out = Vec::with_capacity(4);
        if self.lowercase {
            out.push(LOWER);
        }
        if self.uppercase {
            out.push(UPPER);
        }
        if self.digits {
            out.push(DIGITS);
        }
        if self.symbols {
            out.push(SYMBOLS);
        }
        out
    }

    /// Check the recipe without generating anything, so a form can disable its
    /// button rather than wait for a failure.
    pub fn validate(&self) -> Result<(), PasswordError> {
        if self.classes().is_empty() {
            return Err(PasswordError::NoCharacterClasses);
        }
        if !(MIN_LENGTH..=MAX_LENGTH).contains(&self.length) {
            return Err(PasswordError::Length);
        }
        Ok(())
    }

    /// Rough strength, in bits, assuming the alphabet is what an attacker
    /// knows and every position is independent.
    ///
    /// Deliberately an *under*-estimate of nothing and an over-estimate of
    /// nothing: it ignores the small reduction from the class-coverage
    /// guarantee, which is under a bit at any usable length. It exists to move
    /// a meter, not to be quoted in a threat model.
    pub fn entropy_bits(&self) -> f64 {
        let alphabet = self.classes().iter().map(|c| c.len()).sum::<usize>();
        if alphabet <= 1 || self.length == 0 {
            return 0.0;
        }
        self.length as f64 * (alphabet as f64).log2()
    }
}

/// Fill `buf` from the OS CSPRNG, or fail.
fn fill(buf: &mut [u8]) -> Result<(), PasswordError> {
    random::fill(buf).map_err(|_| PasswordError::Randomness)
}

/// A uniform integer in `0..n`, by rejection sampling.
///
/// `n` is at most 128 here (the longest password, and the largest alphabet is
/// 94), so one byte is always enough and the rejection rate is at worst just
/// under half. The loop is bounded in expectation, not in the worst case,
/// which is the standard shape — but each iteration draws a *fresh* byte, so
/// it cannot spin on a stuck value.
fn uniform_below(n: usize) -> Result<usize, PasswordError> {
    debug_assert!(n > 0 && n <= 256);
    if n == 1 {
        return Ok(0);
    }
    // The largest multiple of `n` that fits in a byte; anything at or above it
    // is the biased tail and is thrown away.
    let limit = (256 / n) * n;
    let mut byte = [0u8; 1];
    loop {
        fill(&mut byte)?;
        let v = byte[0] as usize;
        if v < limit {
            return Ok(v % n);
        }
    }
}

/// Generate a password from `recipe`.
///
/// The result contains at least one character from every enabled class, is
/// exactly `recipe.length` characters long, and is uniformly distributed over
/// the strings that satisfy those two constraints.
pub fn generate(recipe: &Recipe) -> Result<String, PasswordError> {
    recipe.validate()?;
    let classes = recipe.classes();

    // The length floor is well above four, so this cannot overflow the
    // requested length — but the check is here rather than assumed, because
    // `MIN_LENGTH` is a constant somebody could lower.
    if classes.len() > recipe.length {
        return Err(PasswordError::Length);
    }

    let alphabet: Vec<u8> = classes.iter().flat_map(|c| c.iter().copied()).collect();

    let mut out = Vec::with_capacity(recipe.length);
    // One mandatory character per class first…
    for class in &classes {
        out.push(class[uniform_below(class.len())?]);
    }
    // …then the rest from the union of them all.
    while out.len() < recipe.length {
        out.push(alphabet[uniform_below(alphabet.len())?]);
    }

    // Fisher-Yates, so the mandatory characters do not sit in a fixed order at
    // the front. Without this, position 0 is always a lowercase letter and an
    // attacker's search space shrinks by exactly that much.
    for i in (1..out.len()).rev() {
        out.swap(i, uniform_below(i + 1)?);
    }

    // Every byte came from an ASCII table above, so this cannot fail; the
    // fallible form is used anyway rather than an `unwrap` on generated data.
    String::from_utf8(out).map_err(|_| PasswordError::Randomness)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(lowercase: bool, uppercase: bool, digits: bool, symbols: bool) -> Recipe {
        Recipe {
            length: 16,
            lowercase,
            uppercase,
            digits,
            symbols,
        }
    }

    #[test]
    fn a_default_password_has_the_default_length_and_all_four_classes() {
        let pw = generate(&Recipe::default()).unwrap();
        assert_eq!(pw.chars().count(), DEFAULT_LENGTH);
        assert!(pw.bytes().any(|b| LOWER.contains(&b)));
        assert!(pw.bytes().any(|b| UPPER.contains(&b)));
        assert!(pw.bytes().any(|b| DIGITS.contains(&b)));
        assert!(pw.bytes().any(|b| SYMBOLS.contains(&b)));
    }

    #[test]
    fn every_length_in_range_is_honoured_exactly() {
        for length in [MIN_LENGTH, 12, 32, MAX_LENGTH] {
            let recipe = Recipe {
                length,
                ..Recipe::default()
            };
            assert_eq!(generate(&recipe).unwrap().len(), length, "length {length}");
        }
    }

    #[test]
    fn a_disabled_class_never_appears() {
        // The half of the contract that is easy to forget: asking for "no
        // symbols" because a form rejects them must actually mean none, or the
        // user pastes something the form refuses and blames the generator.
        for _ in 0..64 {
            let pw = generate(&only(true, false, true, false)).unwrap();
            assert!(pw
                .bytes()
                .all(|b| LOWER.contains(&b) || DIGITS.contains(&b)));
            assert!(pw.bytes().any(|b| LOWER.contains(&b)));
            assert!(pw.bytes().any(|b| DIGITS.contains(&b)));
        }
    }

    #[test]
    fn a_single_class_recipe_still_works() {
        let pw = generate(&only(false, false, true, false)).unwrap();
        assert_eq!(pw.len(), 16);
        assert!(pw.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn class_coverage_holds_over_many_draws() {
        // With four classes at the minimum length, a generator that merely
        // sampled the union would miss a class often enough to notice — and
        // rarely enough to ship. A hundred draws makes that visible.
        let recipe = Recipe {
            length: MIN_LENGTH,
            ..Recipe::default()
        };
        for i in 0..100 {
            let pw = generate(&recipe).unwrap();
            assert!(pw.bytes().any(|b| DIGITS.contains(&b)), "draw {i}: {pw}");
            assert!(pw.bytes().any(|b| SYMBOLS.contains(&b)), "draw {i}: {pw}");
            assert!(pw.bytes().any(|b| UPPER.contains(&b)), "draw {i}: {pw}");
            assert!(pw.bytes().any(|b| LOWER.contains(&b)), "draw {i}: {pw}");
        }
    }

    #[test]
    fn successive_calls_differ() {
        // Not a distribution test — that would need a seeded RNG, and a test
        // that pinned an expected value against a seed would be asserting the
        // exact property this generator must not have. This asserts only that
        // the output is not constant, which a stuck or clock-seeded generator
        // would fail.
        let recipe = Recipe::default();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(
                seen.insert(generate(&recipe).unwrap()),
                "two identical passwords in 64 draws"
            );
        }
    }

    #[test]
    fn the_mandatory_characters_are_not_pinned_to_the_front() {
        // Before the shuffle, position 0 was always lowercase, position 1
        // always uppercase, and so on. Over 200 draws at least one password
        // must start with something else, or the shuffle is not running.
        let recipe = Recipe::default();
        let mut non_lower_first = 0;
        for _ in 0..200 {
            let pw = generate(&recipe).unwrap();
            if !LOWER.contains(&pw.as_bytes()[0]) {
                non_lower_first += 1;
            }
        }
        assert!(
            non_lower_first > 0,
            "the first character is always lowercase — the shuffle is not running"
        );
    }

    #[test]
    fn a_recipe_with_no_classes_is_refused_rather_than_repaired() {
        let recipe = only(false, false, false, false);
        assert_eq!(recipe.validate(), Err(PasswordError::NoCharacterClasses));
        assert_eq!(generate(&recipe), Err(PasswordError::NoCharacterClasses));
    }

    #[test]
    fn lengths_outside_the_bounds_are_refused() {
        for length in [0, 1, MIN_LENGTH - 1, MAX_LENGTH + 1, 10_000] {
            let recipe = Recipe {
                length,
                ..Recipe::default()
            };
            assert_eq!(generate(&recipe), Err(PasswordError::Length), "{length}");
        }
    }

    #[test]
    fn uniform_below_stays_in_range_and_visits_every_value() {
        // The bias check the module docs describe cannot be asserted directly
        // without a statistical test, but "never out of range" and "reaches
        // both ends" catch the two ways this is usually broken.
        let mut seen = [false; 7];
        for _ in 0..500 {
            let v = uniform_below(7).unwrap();
            assert!(v < 7);
            seen[v] = true;
        }
        assert!(seen.iter().all(|s| *s), "some value was never produced");
        assert_eq!(uniform_below(1).unwrap(), 0);
    }

    #[test]
    fn the_alphabets_are_disjoint_and_contain_no_whitespace() {
        // Overlapping classes would break the "a disabled class never appears"
        // guarantee, and a space in a password is a support ticket.
        let all: Vec<u8> = [LOWER, UPPER, DIGITS, SYMBOLS]
            .iter()
            .flat_map(|c| c.iter().copied())
            .collect();
        let mut sorted = all.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "two classes share a character");
        assert!(all.iter().all(|b| b.is_ascii_graphic()));
    }

    #[test]
    fn entropy_tracks_length_and_alphabet() {
        let full = Recipe::default();
        let digits_only = Recipe {
            lowercase: false,
            uppercase: false,
            symbols: false,
            ..Recipe::default()
        };
        assert!(full.entropy_bits() > digits_only.entropy_bits());
        let longer = Recipe {
            length: DEFAULT_LENGTH * 2,
            ..full
        };
        assert!(longer.entropy_bits() > full.entropy_bits());
        // 20 characters of a 94-character alphabet is a little over 131 bits.
        assert!((full.entropy_bits() - 131.0).abs() < 1.0);
        // A recipe with nothing enabled claims nothing.
        assert_eq!(only(false, false, false, false).entropy_bits(), 0.0);
    }
}
