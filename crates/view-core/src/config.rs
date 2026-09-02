//! The provenance vocabulary every config layer answers in.
//!
//! Two crates resolve `view.toml`: `view-native` owns the tables it parses
//! and `view-ai` owns `[ai]`, and the two are forbidden to each other in
//! both directions. A report that lists every key beside where its answer
//! came from therefore needs one word list both can speak, and this is the
//! only place either can reach for it.

/// Where a resolved value came from, in the precedence order spec section
/// 11 states: a command-line flag beats an environment variable, which
/// beats the config file, which beats the value view derives on its own.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A flag on this invocation's command line.
    Flag,
    /// A `VIEW_*` variable the key registry generates the name of.
    Env,
    /// The `view.toml` this session read.
    File,
    /// Nothing named it, so view answered for itself.
    Derived,
}

impl Source {
    /// The word a report prints for this layer.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Env => "environment",
            Self::File => "config file",
            Self::Derived => "derived",
        }
    }
}

/// What a value that is not a switch owes the user.
pub const BOOL_EXPECTED: &str = "true or false";

/// What a value that names no tier owes the user.
pub const TIER_EXPECTED: &str = "one of auto, full, standard or basic";

/// What a sidebar width that is not a whole number owes the user.
pub const WIDTH_EXPECTED: &str = "a whole number of percent";

/// What a value naming no key this build can match owes the user.
pub const KEYS_EXPECTED: &str =
    "key notations spelled as nvim spells them, case included (\"<C-w>>\", \"<S-Right>\"), at \
     most two keys each, separated by spaces";

/// What a value naming neither review target owes the user.
pub const OPEN_TARGET_EXPECTED: &str = "\"current\" or \"split\"";

/// The line a discarded environment value owes the user: what was set, what
/// it had to be, and which key answers from somewhere else because of it.
///
/// One sentence for both resolvers rather than one each. A user reading two
/// notices in the same startup should not be able to tell which crate wrote
/// which, and two format strings a crate apart drift the moment either is
/// reworded.
#[must_use]
pub fn discarded_env(name: &str, value: &str, expected: &str, table: &str, key: &str) -> String {
    format!(
        "view: {name}={value} is not {expected} -- [{table}] {key} answers from the layer below \
         it this run"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A report prints one word per layer, and two layers sharing a word
    /// would make the column it sits in unreadable.
    #[test]
    fn every_layer_prints_a_word_of_its_own() {
        let labels = [Source::Flag, Source::Env, Source::File, Source::Derived].map(Source::label);
        let mut unique = labels.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
        assert!(labels.iter().all(|word| !word.is_empty()));
    }

    /// The notice carries all four facts a user needs: what they set, what
    /// it said, what it had to say, and which key went somewhere else for
    /// its answer.
    #[test]
    fn a_discarded_value_names_what_was_set_and_what_it_cost() {
        let notice = discarded_env("VIEW_UI_TIER", "turbo", TIER_EXPECTED, "ui", "tier");
        for fact in ["VIEW_UI_TIER", "turbo", TIER_EXPECTED, "[ui] tier"] {
            assert!(notice.contains(fact), "{fact} is missing from {notice:?}");
        }
    }
}
