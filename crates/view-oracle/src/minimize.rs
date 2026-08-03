//! Notation-token-aware delta debugging (ddmin) over an input key-notation
//! script, plus the tokenizer/joiner that turns a script string into the
//! token vector ddmin reduces and back. Pure: no TOML, no engine, no clock
//! -- every reduction candidate's pass/fail verdict comes from a
//! caller-supplied closure, so this module stays serde-free and testable
//! against a fake runner instead of a real nvim (see this module's own
//! tests).
//!
//! [`ddmin`] implements the classic Zeller/Hildebrandt algorithm: split the
//! current token list into `n` chunks, try each chunk alone, then each
//! chunk's complement, and only grow `n` (down to single-token chunks) once
//! neither pass reduces anything. Every candidate the algorithm considers is
//! a contiguous sub-slice or a contiguous-complement of the current list, so
//! the total candidate count -- and therefore the number of (expensive)
//! `test` calls -- is bounded by the token count rather than open-ended: the
//! outer loop can only ever grow `n` up to `tokens.len()`, at which point
//! chunk size hits 1 and a further failed pass ends the loop. A result cache
//! keyed by the exact candidate slice sits in front of every `test` call so
//! ddmin's own complement/regrowth passes -- which can and do re-propose an
//! already-tried candidate -- never re-run a caller's (real: two spawned
//! nvim sessions) probe for the same input twice.

use std::collections::HashMap;

/// Splits `input` into key-notation tokens: each `<...>` escape (`<Esc>`,
/// `<C-d>`, `<Cmd>...<CR>`) is one token, every other character is its own
/// token. Splitting on individual characters (not words) is what lets
/// [`ddmin`] discover a single-character-level minimal reproduction rather
/// than being stuck at whatever line/word granularity a coarser tokenizer
/// would impose.
///
/// A `<` with no closing `>` before the end of `input` degrades to one
/// token per remaining character (including the unmatched `<` itself)
/// rather than swallowing the rest of the string as one token: this keeps
/// [`join_tokens`] a lossless inverse of this function even on malformed
/// input, which the round-trip property this module's own tests check
/// depends on.
#[must_use]
pub fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            tokens.push(c.to_string());
            continue;
        }
        let mut pending = String::from(c);
        let mut closed = false;
        for next in chars.by_ref() {
            pending.push(next);
            if next == '>' {
                closed = true;
                break;
            }
        }
        if closed {
            tokens.push(pending);
        } else {
            tokens.extend(pending.chars().map(|ch| ch.to_string()));
        }
    }
    tokens
}

/// Concatenates `tokens` back into one script string -- the exact inverse
/// of [`tokenize`] (see that function's own doc comment for the round-trip
/// guarantee), and the join every minimizer/fuzz candidate uses to turn a
/// token vector back into the string a real `nvim_input` call takes.
#[must_use]
pub fn join_tokens(tokens: &[String]) -> String {
    tokens.concat()
}

/// Runs `test` against `candidate`, consulting/populating `cache` first so
/// an identical candidate (by value, not by which pass proposed it) is
/// never probed twice -- see this module's own doc comment for why ddmin's
/// complement/regrowth passes make that a real, not theoretical, cost.
fn cached_test<F>(
    cache: &mut HashMap<Vec<String>, bool>,
    test: &mut F,
    candidate: &[String],
) -> bool
where
    F: FnMut(&[String]) -> bool,
{
    if let Some(&hit) = cache.get(candidate) {
        return hit;
    }
    let result = test(candidate);
    cache.insert(candidate.to_vec(), result);
    result
}

/// Reduces `tokens` to a locally 1-minimal subsequence `test` still reports
/// as reproducing (returns `true` for): removing any single remaining token
/// makes `test` return `false`. `test` is the reproduction predicate --
/// true means "this candidate still exhibits the target failure" -- and is
/// the only thing this function knows about *what* failure is being
/// minimized toward; a caller wiring this against a real oracle run
/// supplies a closure that reruns the two-session comparison and checks the
/// specific divergence (or timeout) signature it started from, not just
/// "any failure at all" (see `crates/view-harness/src/bin/oracle.rs`'s
/// `FailureSignature`).
///
/// Terminates in a bounded number of `test` calls: each outer iteration
/// either shrinks `tokens` (bounding total shrink steps by the starting
/// length) or grows the chunk count `n` (bounded above by `tokens.len()`,
/// at which point every chunk is a single token and a further failed pass
/// ends the loop) -- the standard ddmin termination argument, not a retry
/// loop bounded by a timeout or an attempt counter.
#[must_use]
pub fn ddmin<F>(tokens: Vec<String>, mut test: F) -> Vec<String>
where
    F: FnMut(&[String]) -> bool,
{
    let mut cache: HashMap<Vec<String>, bool> = HashMap::new();
    let mut current = tokens;
    let mut n: usize = 2;

    while current.len() >= 2 {
        let len = current.len();
        let chunk_size = len.div_ceil(n);
        let mut reduced = false;

        for start in (0..len).step_by(chunk_size) {
            let end = (start + chunk_size).min(len);
            let chunk = &current[start..end];
            if chunk.len() == len {
                continue;
            }
            if cached_test(&mut cache, &mut test, chunk) {
                current = chunk.to_vec();
                n = (n - 1).max(2);
                reduced = true;
                break;
            }
        }

        if !reduced {
            for start in (0..len).step_by(chunk_size) {
                let end = (start + chunk_size).min(len);
                let mut complement = Vec::with_capacity(len - (end - start));
                complement.extend_from_slice(&current[..start]);
                complement.extend_from_slice(&current[end..]);
                if complement.is_empty() || complement.len() == len {
                    continue;
                }
                if cached_test(&mut cache, &mut test, &complement) {
                    current = complement;
                    n = (n - 1).max(2);
                    reduced = true;
                    break;
                }
            }
        }

        if !reduced {
            if n >= len {
                break;
            }
            n = (n * 2).min(len);
        }
    }

    current
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn divergence_iff_token_x_present_minimizes_to_exactly_x() {
        let tokens = tokenize("abcXdefg");
        let result = ddmin(tokens, |candidate| candidate.iter().any(|t| t == "X"));
        assert_eq!(result, vec!["X".to_string()]);
    }

    #[test]
    fn divergence_iff_a_then_b_minimizes_to_a_then_b() {
        let tokens = tokenize("1a2b3");
        let result = ddmin(tokens, |candidate| {
            let pos_a = candidate.iter().position(|t| t == "a");
            let pos_b = candidate.iter().position(|t| t == "b");
            matches!((pos_a, pos_b), (Some(pa), Some(pb)) if pa < pb)
        });
        assert_eq!(result, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_test_that_never_reproduces_leaves_the_input_unreduced() {
        // ddmin does not itself verify that the starting (full) candidate
        // reproduces before searching for a smaller one; given a predicate
        // that never once returns true, not even for the full set, no
        // candidate ever looks smaller-and-still-reproducing, so the
        // algorithm must terminate having made zero changes rather than
        // fabricate a reduction (or loop forever) hunting for one that
        // will never come.
        let tokens = tokenize("abcdef");
        let expected = tokens.clone();
        let result = ddmin(tokens, |_candidate| false);
        assert_eq!(result, expected);
    }

    #[test]
    fn a_test_that_always_reproduces_reduces_to_a_single_token() {
        let tokens = tokenize("abcdef");
        let result = ddmin(tokens, |_candidate| true);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn a_shared_result_cache_never_probes_the_same_candidate_twice() {
        let tokens = tokenize("abcd");
        let mut calls: Vec<Vec<String>> = Vec::new();
        let result = ddmin(tokens, |candidate| {
            calls.push(candidate.to_vec());
            candidate.contains(&"c".to_string())
        });
        assert_eq!(result, vec!["c".to_string()]);
        let unique: std::collections::HashSet<_> = calls.iter().cloned().collect();
        assert_eq!(
            calls.len(),
            unique.len(),
            "expected every probed candidate to be distinct, got {calls:?}"
        );
    }

    #[test]
    fn tokenize_splits_notation_escapes_as_single_tokens() {
        assert_eq!(
            tokenize("ihello world<Esc>0x"),
            vec!["i", "h", "e", "l", "l", "o", " ", "w", "o", "r", "l", "d", "<Esc>", "0", "x",]
        );
    }

    #[test]
    fn tokenize_degrades_an_unclosed_escape_to_single_char_tokens() {
        assert_eq!(tokenize("a<bc"), vec!["a", "<", "b", "c"]);
    }

    #[test]
    fn join_tokens_is_the_inverse_of_tokenize_for_every_seed_input() {
        let samples = [
            "ihello world<Esc>0x",
            "\"ayy\"ap",
            "vjy P",
            "100Gzz<C-d><C-d>",
            "/pattern<CR>n",
            ":tabnew<CR>gt",
            "",
        ];
        for sample in samples {
            assert_eq!(join_tokens(&tokenize(sample)), sample);
        }
    }
}
