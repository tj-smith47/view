//! Seeded fuzz-script generation: a deterministic PRNG plus a weighted
//! key-notation alphabet, producing the token vectors the `fuzz` subcommand
//! feeds through the oracle stack. Pure -- no engine, no clock, no file I/O
//! -- so a script's exact contents can be reproduced and inspected without
//! spawning nvim (see this module's own tests).
//!
//! A hand-rolled PRNG rather than a `rand`-crate dependency: the corpus
//! discipline this generator exists to serve requires a `--seed` to
//! reproduce byte-identical scripts across every future run of this binary,
//! which only an algorithm pinned in this crate's own source -- not
//! whatever a dependency's internals happen to do at whatever version
//! resolves at build time -- can promise indefinitely.

/// SplitMix64 (Vigna): a fast, small, well-known deterministic generator
/// (also the algorithm behind Java's `SplittableRandom`), good enough for
/// generating test scripts and, unlike a cryptographic RNG, trivial to
/// re-implement identically forever without a dependency.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform value in `0..bound` via Lemire's rejection method: `%`
    /// alone biases toward small results whenever `bound` does not evenly
    /// divide `u64::MAX + 1`, which every alphabet-length draw here hits
    /// (the alphabet size is never a power of two).
    fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0, "below() requires a positive bound");
        let mut x = self.next_u64();
        let mut wide = u128::from(x) * u128::from(bound);
        let mut low = wide as u64;
        if low < bound {
            let threshold = bound.wrapping_neg() % bound;
            while low < threshold {
                x = self.next_u64();
                wide = u128::from(x) * u128::from(bound);
                low = wide as u64;
            }
        }
        (wide >> 64) as u64
    }
}

/// The fuzz generator's key-notation alphabet, grouped by the categories a
/// realistic editing session draws from: printable insert-mode content,
/// motions, operators, mode switches, registers, cmdline entry, and window
/// operations. Weights bias toward movement/insertion/mode-exit tokens
/// (the ones most likely to produce a script that still settles) over
/// register/cmdline/window tokens (rarer in practice, and individually more
/// likely to leave a session in a state neither side settles from inside
/// the quiesce deadline -- itself a reachable, intentional outcome this
/// generator does not try to engineer away, only tilt against).
const ALPHABET: &[(&str, u32)] = &[
    // printable insert-mode content
    ("a", 6),
    ("b", 6),
    ("c", 6),
    ("d", 6),
    ("e", 6),
    ("f", 6),
    ("g", 6),
    ("h", 6),
    ("i", 6),
    ("j", 6),
    ("k", 6),
    ("l", 6),
    ("m", 6),
    ("n", 6),
    ("o", 6),
    ("p", 6),
    ("q", 6),
    ("r", 6),
    ("s", 6),
    ("t", 6),
    ("u", 6),
    ("v", 6),
    ("w", 6),
    ("x", 6),
    ("y", 6),
    ("z", 6),
    (" ", 8),
    (".", 3),
    ("0", 3),
    ("1", 3),
    ("!", 1),
    // motions
    ("h", 5),
    ("j", 5),
    ("k", 5),
    ("l", 5),
    ("w", 5),
    ("b", 5),
    ("e", 5),
    ("0", 5),
    ("$", 5),
    ("gg", 2),
    ("G", 2),
    // operators (including doubled-operator forms)
    ("d", 5),
    ("c", 5),
    ("y", 5),
    ("p", 5),
    ("dd", 3),
    ("yy", 3),
    ("cc", 2),
    ("dw", 3),
    ("x", 5),
    // mode switches
    ("i", 5),
    ("a", 5),
    ("o", 5),
    ("O", 3),
    ("<Esc>", 12),
    ("v", 3),
    ("V", 2),
    ("<C-v>", 1),
    ("R", 1),
    // registers
    ("\"a", 2),
    ("\"0", 2),
    ("\"1", 1),
    // cmdline entry
    (":", 2),
    ("/", 1),
    ("?", 1),
    ("<CR>", 6),
    // window operations
    ("<C-w>s", 1),
    ("<C-w>v", 1),
    ("gt", 1),
    ("gT", 1),
];

/// `ALPHABET`'s total weight, computed once at compile time: [`below`]'s
/// bound for [`pick_token`]'s weighted draw.
const fn total_weight() -> u64 {
    let mut total: u64 = 0;
    let mut i = 0;
    while i < ALPHABET.len() {
        total += ALPHABET[i].1 as u64;
        i += 1;
    }
    total
}
const TOTAL_WEIGHT: u64 = total_weight();

/// Draws one token from [`ALPHABET`], weighted.
fn pick_token(rng: &mut SplitMix64) -> &'static str {
    let mut roll = rng.below(TOTAL_WEIGHT);
    for (token, weight) in ALPHABET {
        let weight = u64::from(*weight);
        if roll < weight {
            return token;
        }
        roll -= weight;
    }
    // Unreachable given `roll < TOTAL_WEIGHT` by `below`'s own contract;
    // degrades to the first entry rather than panicking on a fuzz-facing
    // path if that invariant is ever violated.
    ALPHABET.first().map_or("", |(token, _)| token)
}

/// Generates round `round`'s `keys`-token script for `seed`, independent of
/// every other round: `SplitMix64::new(seed)` is advanced `round` steps to
/// derive that round's own substream seed, then a fresh generator drawn
/// from it produces `keys` tokens. Deterministic in both `seed` and
/// `round`, and independent per round by construction -- `oracle fuzz
/// --seed 42` regenerating round 17 in isolation reproduces the exact
/// script that round produced inside a full `--rounds 200` run, without
/// replaying rounds 0-16 first.
#[must_use]
pub fn generate_round(seed: u64, round: u32, keys: usize) -> Vec<String> {
    let mut mixer = SplitMix64::new(seed);
    for _ in 0..round {
        mixer.next_u64();
    }
    let mut rng = SplitMix64::new(mixer.next_u64());
    (0..keys)
        .map(|_| pick_token(&mut rng).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_same_seed_and_round_reproduce_a_byte_identical_script() {
        let first = generate_round(42, 17, 150);
        let second = generate_round(42, 17, 150);
        assert_eq!(first, second);
    }

    #[test]
    fn a_full_run_reproduces_across_two_independent_invocations() {
        // The scenario `fuzz --seed N` must satisfy end to end: replaying
        // every round of a full run, not just one round in isolation.
        let seed = 7;
        let rounds = 20;
        let run_a: Vec<Vec<String>> = (0..rounds).map(|r| generate_round(seed, r, 40)).collect();
        let run_b: Vec<Vec<String>> = (0..rounds).map(|r| generate_round(seed, r, 40)).collect();
        assert_eq!(run_a, run_b);
    }

    #[test]
    fn different_seeds_produce_different_scripts() {
        let a = generate_round(1, 0, 50);
        let b = generate_round(2, 0, 50);
        assert_ne!(a, b, "distinct seeds collided on the same script");
    }

    #[test]
    fn different_rounds_under_the_same_seed_produce_different_scripts() {
        let round_0 = generate_round(9, 0, 50);
        let round_1 = generate_round(9, 1, 50);
        assert_ne!(
            round_0, round_1,
            "distinct rounds collided on the same script"
        );
    }

    #[test]
    fn generated_tokens_always_come_from_the_alphabet() {
        let known: std::collections::HashSet<&str> = ALPHABET.iter().map(|(t, _)| *t).collect();
        for round in 0..10 {
            for token in generate_round(123, round, 60) {
                assert!(known.contains(token.as_str()), "unexpected token {token:?}");
            }
        }
    }

    #[test]
    fn requested_key_count_is_honored_exactly() {
        assert_eq!(generate_round(5, 0, 0).len(), 0);
        assert_eq!(generate_round(5, 0, 1).len(), 1);
        assert_eq!(generate_round(5, 0, 150).len(), 150);
    }
}
