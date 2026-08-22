//! Which build of view each bench scenario measures, and the flag that
//! names that build on the bench command line.
//!
//! One table, in the lib rather than beside the bench binary's own row
//! dispatch, because two callers have to agree on it: the bench binary
//! refuses a `--*-view-bin` flag the selected rows would not read, and the
//! heartbeat armed-vs-absent campaign picks the flag it drives each of its
//! rows with. A campaign naming a flag the row does not read measures the
//! default build under both arms and reports a difference it never
//! measured -- the defect this table exists to make unrepresentable.

/// The shipped release binary, which a row measures unless its own
/// boundary needs a bench arm.
pub const VIEW_BIN: &str = "--view-bin";

/// The `bench-taps` build, whose intervals are read off the tap channel.
pub const TAPS_VIEW_BIN: &str = "--taps-view-bin";

/// The `bench-no-speculate` build, which predicts nothing, so a row
/// closing on a glyph times the engine round trip rather than the paint a
/// prediction put there first.
pub const NOSPEC_VIEW_BIN: &str = "--nospec-view-bin";

/// The `bench-taps` + `bench-no-speculate` build, for the row that
/// decomposes an echo round trip through the taps.
pub const TAPS_NOSPEC_VIEW_BIN: &str = "--taps-nospec-view-bin";

/// Every `--*-view-bin` flag the bench binary accepts, so a check over
/// "which named binary went unread" can walk them all rather than the one
/// that happened to be noticed.
pub const VIEW_BIN_FLAGS: &[&str] = &[
    VIEW_BIN,
    TAPS_VIEW_BIN,
    NOSPEC_VIEW_BIN,
    TAPS_NOSPEC_VIEW_BIN,
];

/// The flag naming the build each scenario measures, `None` where the pair
/// spawns no view build at all.
///
/// Enumerated with no wildcard: a default arm mapping every unnamed
/// scenario to the shipped build hands a new arm-measured row the wrong
/// answer silently, and the checks reading this table would then accept a
/// run whose numbers came from a binary nobody named. A scenario missing
/// from this table is a table bug, which is why [`names_a_build`] can tell
/// "absent" from "spawns nothing".
pub const MEASURED_BUILD: &[(&str, Option<&str>)] = &[
    ("echo", Some(NOSPEC_VIEW_BIN)),
    ("echo_path", Some(TAPS_NOSPEC_VIEW_BIN)),
    ("echo_speculated", Some(TAPS_VIEW_BIN)),
    ("input_path", Some(TAPS_VIEW_BIN)),
    ("output_path", Some(TAPS_VIEW_BIN)),
    // the same two boundaries, with an agent turn streaming underneath
    ("ai_session_active", Some(TAPS_VIEW_BIN)),
    ("ai_streaming", Some(TAPS_VIEW_BIN)),
    // the panel's own composer, with no turn under it
    ("ai_composer", Some(TAPS_VIEW_BIN)),
    ("first_paint", Some(VIEW_BIN)),
    ("scroll", Some(VIEW_BIN)),
    ("memory", Some(VIEW_BIN)),
    ("remote_memory", Some(VIEW_BIN)),
    ("flood", Some(VIEW_BIN)),
    ("picker", Some(VIEW_BIN)),
    ("supervision", Some(VIEW_BIN)),
    // both sides are bare nvim: the control arm measures a remote UI
    // attached to a headless server, with no view build in the pair
    ("echo_control", None),
];

/// Whether [`MEASURED_BUILD`] names `scenario` at all.
#[must_use]
pub fn names_a_build(scenario: &str) -> bool {
    MEASURED_BUILD.iter().any(|(name, _)| *name == scenario)
}

/// Which `--*-view-bin` flag names the build `scenario` measures.
///
/// `None` both for a row that spawns no view binary and for a scenario the
/// table does not name; [`names_a_build`] separates the two.
#[must_use]
pub fn view_bin_flag(scenario: &str) -> Option<&'static str> {
    MEASURED_BUILD
        .iter()
        .find(|(name, _)| *name == scenario)
        .and_then(|(_, flag)| *flag)
}

/// The scenarios that read `flag`, in table order.
#[must_use]
pub fn scenarios_reading(flag: &str) -> Vec<&'static str> {
    MEASURED_BUILD
        .iter()
        .filter(|(_, named)| *named == Some(flag))
        .map(|(name, _)| *name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scenario named twice would let the two entries disagree, and
    /// every reader here takes the first match.
    #[test]
    fn no_scenario_is_named_twice() {
        let mut names: Vec<&str> = MEASURED_BUILD.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a scenario is named twice: {names:?}");
    }

    /// A flag in the table that the bench binary does not accept would be
    /// printed in a refusal telling an operator to pass something that
    /// parses as an error.
    #[test]
    fn every_named_flag_is_one_the_bench_binary_accepts() {
        for (scenario, flag) in MEASURED_BUILD {
            let Some(flag) = flag else {
                continue;
            };
            assert!(
                VIEW_BIN_FLAGS.contains(flag),
                "{scenario} names {flag}, which is not a bench flag"
            );
        }
    }

    #[test]
    fn a_scenario_absent_from_the_table_is_distinguishable_from_one_that_spawns_nothing() {
        assert!(names_a_build("echo_control"));
        assert_eq!(view_bin_flag("echo_control"), None);
        assert!(!names_a_build("no_such_scenario"));
        assert_eq!(view_bin_flag("no_such_scenario"), None);
    }

    #[test]
    fn the_rows_reading_a_flag_are_the_rows_the_table_gives_it_to() {
        assert_eq!(scenarios_reading(NOSPEC_VIEW_BIN), vec!["echo"]);
        assert_eq!(
            scenarios_reading(TAPS_VIEW_BIN),
            vec![
                "echo_speculated",
                "input_path",
                "output_path",
                "ai_session_active",
                "ai_streaming",
                "ai_composer"
            ]
        );
        assert!(scenarios_reading(VIEW_BIN).contains(&"picker"));
    }
}
