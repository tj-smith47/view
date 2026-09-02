//! Source-text pins on how the workspace's tests are allowed to assert
//! time.
//!
//! Two claims, neither of which any single timing test can make about
//! itself: that the constant a budget is derived from still says what the
//! copy here says it does, and that no later test in any crate reintroduces
//! the shape those budgets replaced -- a hand-picked absolute wall clock,
//! which passes or fails on what else the host was doing.
//!
//! Housed here rather than in each crate because the rule is one rule and a
//! copy of it per crate is a rule that drifts. This crate's tests already
//! walk source text for a living; nothing about the walk needs the oracle
//! itself.
//!
//! The walk covers every `crates/*/tests/**/*.rs` outright, and the
//! `#[cfg(test)]` region of every `crates/*/src/**/*.rs` -- test code lives
//! in both places in this tree, and a rule that reached only one of them
//! would be a rule half the population never meets. It reads statements
//! rather than lines, because rustfmt wraps a long assertion across four of
//! them and a line-at-a-time reader sees none of the shapes it is looking
//! for.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// This file's own name, which the walk below skips.
const SELF_SOURCE: &str = "timing_bounds.rs";

/// Where view-tui defines the constant `common::PROBE_DEADLINE` copies,
/// relative to this crate's own `tests` directory.
const TIERS_SOURCE: &str = "../../view-tui/src/tiers.rs";

/// A bound on a measured span that is allowed to stay an absolute.
///
/// Every entry is a wall clock that means something other than how fast the
/// host is, which is why scaling it with the host's load would make it
/// worse: a bound that sits below a competing behaviour stops telling the
/// two apart, a liveness bound stops being one, and a runaway guard becomes
/// a longer-lived stray.
///
/// Keyed by the statement's text, which must occur exactly once in the
/// file: a second copy of the same line cannot quietly inherit an exemption
/// nobody granted it, and no unrelated edit above the line moves the key.
struct DeclaredAbsolute {
    /// The path under `crates/`, as the walk below names what it finds.
    file: &'static str,
    /// The bound's own text, trimmed, which must appear exactly once.
    line: &'static str,
    /// Why this one is a real timing rather than a guess at the host, for
    /// the reader who finds the entry before finding the test.
    grounds: &'static str,
}

const DECLARED_ABSOLUTES: &[DeclaredAbsolute] = &[
    DeclaredAbsolute {
        file: "view-oracle/tests/osc52_user_provider_paste.rs",
        line: "elapsed < Duration::from_secs(2),",
        grounds: "it sits below nvim's own OSC 52 provider waiting out its first vim.wait(1000)",
    },
    DeclaredAbsolute {
        file: "view-oracle/tests/osc52_user_provider_paste.rs",
        line: "elapsed < Duration::from_secs(3),",
        grounds: "it sits below the same provider's wait, for the empty-clipboard answer",
    },
    DeclaredAbsolute {
        file: "view-native/src/tree/git.rs",
        line: "elapsed < Duration::from_secs(2),",
        grounds:
            "it sits below the wedged fixture's own 5s sleep, which a scaled bound would reach",
    },
    DeclaredAbsolute {
        file: "view-ai/tests/fixtures/stub_agent.rs",
        line: "while start.elapsed() < SUSTAINED_CEILING {",
        grounds: "it is the fixture's own runaway guard, and a scaled one is \
                  just a longer-lived stray process",
    },
    DeclaredAbsolute {
        file: "view-ai/tests/fixtures/drop_harness.rs",
        line: "if rx.recv_timeout(READY).is_err() {",
        grounds: "the fixture is a binary rather than a test, so it links \
                  none of the dev-only crate that scales a bound, and what \
                  it waits out is a handshake that either happens or does \
                  not",
    },
    DeclaredAbsolute {
        file: "view/src/runtime.rs",
        line: "elapsed >= std::time::Duration::from_millis(60),",
        grounds: "it sits above one coalesce window and below the re-probe \
                  grace, and a scaled one straddles the grace it is there \
                  to stay under",
    },
    DeclaredAbsolute {
        file: "view/src/runtime.rs",
        line: "if entered.duration_since(dispatched) >= std::time::Duration::from_millis(60) {",
        grounds: "it asks whether this thread stayed awake across a path \
                  whose own cost is microseconds, and a bound that grew \
                  with the host's load would accept the stalled sample it \
                  exists to throw away",
    },
    DeclaredAbsolute {
        file: "view/src/runtime.rs",
        line: "observed_for >= std::time::Duration::from_millis(20),",
        grounds: "it restates the sleep two lines above it, and the \
                  episode's clock opened before that sleep, so a slow host \
                  can only overshoot the floor -- the only reading that \
                  fails is a readout that went backwards",
    },
    DeclaredAbsolute {
        file: "view/src/remote_guard.rs",
        line: "waited < Duration::from_secs(5),",
        grounds: "it discriminates between the 200ms bound under test and \
                  the 30s the fixture client sits for, and sits an order of \
                  magnitude from each, so the host would have to be slower \
                  than the failure it is telling apart",
    },
    DeclaredAbsolute {
        file: "view-native/src/picker/matcher.rs",
        line:
            "if state != last_state || now.duration_since(last_entry) >= Duration::from_secs(5) {",
        grounds: "it throttles how often a hot diagnostic loop records a \
                  line, so what it bounds is the size of the output rather \
                  than any verdict, and scaling it with the host would \
                  thin the record exactly when the record is wanted",
    },
    DeclaredAbsolute {
        file: "view/tests/supervision_live.rs",
        line: "if quiet_since.elapsed() >= bound + WATCHDOG_MARGIN {",
        grounds: "the margin past the derived bound is the runaway guard \
                  that ends a wait the watch failed to bound, so scaling it \
                  with the host only makes the stray it catches live longer",
    },
    DeclaredAbsolute {
        file: "view-engine/tests/shutdown.rs",
        line: "let deadline = std::time::Instant::now() + GRACEFUL_EXIT_LIVENESS_BOUND;",
        grounds: "a child that has not run at all in a minute is not a \
                  descheduled child, which is the whole of what the bound claims",
    },
    DeclaredAbsolute {
        file: "view-engine/tests/shutdown.rs",
        line: ".recv_timeout(GRACEFUL_EXIT_LIVENESS_BOUND)",
        grounds: "it is the same liveness bound the deadline above it is, \
                  spent waiting rather than compared",
    },
    DeclaredAbsolute {
        file: "view-oracle/src/reference.rs",
        line: "started.elapsed() < QUIESCE_DEADLINE,",
        grounds: "it is the deadline quiesce itself was handed, which the \
                  assertion exists to prove was not exhausted",
    },
];

#[test]
fn the_probe_constant_copies_still_match_view_tuis_own() {
    let source = std::fs::read_to_string(crate_relative(TIERS_SOURCE))
        .expect("view-tui's tiers.rs must be readable from this crate");
    // every constant this crate copies from the probe, walked rather than
    // asserted one by one: the next copy is held by adding a row
    for (name, value) in [
        ("PROBE_DEADLINE", common::PROBE_DEADLINE),
        ("PROBE_HARD_CAP", common::PROBE_HARD_CAP),
    ] {
        let expected = format!(
            "pub const {name}: Duration = Duration::from_millis({});",
            value.as_millis()
        );
        assert!(
            source.contains(&expected),
            "common::{name} reads {value:?}, which {TIERS_SOURCE} no longer \
             declares. This crate may not depend on view-tui, so the copy is \
             held here instead of by the compiler: update it, and with it \
             every startup budget derived from it, to whatever the \
             definition now says"
        );
    }
}

#[test]
fn no_timing_test_bounds_a_measured_span_with_an_undeclared_absolute() {
    let mut undeclared = Vec::new();
    for (name, source) in common::workspace_test_sources() {
        if name.ends_with(SELF_SOURCE) {
            // this file quotes the very shapes it forbids, in
            // DECLARED_ABSOLUTES and in the fixture below
            continue;
        }
        for found in absolute_span_bounds(&source, &common::whole_source(&name)) {
            if is_declared(&name, &found) {
                continue;
            }
            undeclared.push(format!("{name}:{}: {}", found.number, found.line));
        }
    }
    assert!(
        undeclared.is_empty(),
        "these assertions bound a measured span with a hand-picked absolute, \
         which fails on a loaded host without saying anything about view:\n  \
         {}\nDerive the bound from the constants of the code under test and \
         let view_test_support::HostBudget scale the host's share (see \
         common::startup_budget here, or view-engine's own rpc_deadline), or \
         -- if the wall clock really does mean something other than how fast \
         the host is -- add it to DECLARED_ABSOLUTES with the grounds that \
         make it one",
        undeclared.join("\n  ")
    );
}

/// The shapes that once escaped the walk, each of which it must now see.
///
/// A rule proved only by a population that currently satisfies it is a rule
/// that cannot tell "nothing is wrong" from "nothing is being read". This
/// fixture is wrong on purpose, one way per shape.
const ESCAPING_SHAPES: &str = r#"
#[cfg(test)]
mod tests {
    const INTERRUPTED: Duration = Duration::from_secs(5);
    const SETTLE: Duration = view_test_support::host_deadline(Duration::from_secs(5));

    #[test]
    fn a_named_constant_is_still_a_hand_picked_bound() {
        assert!(start.elapsed() < INTERRUPTED, "not an abort");
    }

    #[test]
    fn rustfmt_wrapping_hides_nothing() {
        assert!(
            elapsed
                < Duration::from_secs(4),
            "took {elapsed:?}"
        );
    }

    #[test]
    fn a_deadline_is_the_same_wall_clock_by_another_name() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {}
    }

    #[test]
    fn an_inclusive_bound_is_still_a_bound() {
        assert!(elapsed <= Duration::from_secs(4), "took {elapsed:?}");
    }

    #[test]
    fn the_same_bound_reads_the_same_backwards() {
        assert!(Duration::from_secs(4) > elapsed, "took {elapsed:?}");
    }

    #[test]
    fn a_floor_is_the_same_wall_clock_with_the_sides_swapped() {
        assert!(elapsed >= Duration::from_millis(60), "took {elapsed:?}");
    }

    #[test]
    fn a_floor_on_a_span_the_list_names_is_still_a_bound() {
        assert!(observed_for >= Duration::from_millis(20), "the episode clock");
    }

    #[test]
    fn converting_the_span_hides_the_type_not_the_clock() {
        assert!(start.elapsed().as_millis() < 50, "took {elapsed:?}");
    }

    #[test]
    fn a_timeout_spends_the_same_wall_clock_without_comparing_it() {
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (_held, timed_out) = cvar.wait_timeout(guard, INTERRUPTED).unwrap();
    }

    #[test]
    fn these_are_the_shapes_the_rule_asks_for() {
        assert!(
            elapsed < view_test_support::host_deadline(Duration::from_secs(2)),
            "scaled"
        );
        assert!(elapsed < Duration::from_secs(WAIT_WATCHDOG_SECS), "derived");
        assert!(elapsed < timeout + view_test_support::host_deadline(slack), "split");
        let ceiling = wait_for(&rx, Duration::from_secs(5));
        let deadline = Instant::now() + common::rpc_deadline();
        let settle = Instant::now() + SETTLE;
        let answer = rx.recv_timeout(view_test_support::host_deadline(Duration::from_secs(2)));
        let held = cvar.wait_timeout(guard, SETTLE).unwrap();
    }

    #[test]
    fn a_span_named_outside_the_list_is_where_this_walk_stops() {
        assert!(spent < Duration::from_secs(4), "an unlisted span");
    }
}
"#;

#[test]
fn the_walk_sees_every_shape_a_line_at_a_time_reader_missed() {
    let found: Vec<usize> = absolute_span_bounds(ESCAPING_SHAPES, ESCAPING_SHAPES)
        .iter()
        .map(|found| found.number)
        .collect();
    // the ten deliberately wrong lines: the named constant, the
    // rustfmt-wrapped comparison, the deadline built from `now`, the
    // inclusive bound, the same bound written backwards, the floor with
    // its sides swapped, the floor on a span the list names beyond the
    // two it started with, the one whose span is converted to a number
    // first, and the two waits handed a wall clock they spend rather
    // than compare.
    //
    // The tail of the fixture is the other half of the pin: no line from
    // `these_are_the_shapes_the_rule_asks_for` or from the unlisted span
    // below it may appear, and an expected vector that grew a line is the
    // walk reporting a shape the rule allows.
    assert_eq!(
        found,
        vec![9, 16, 23, 29, 34, 39, 44, 49, 54, 55],
        "the walk read {found:?} of the fixture. Every line it missed is a \
         shape the population can carry unnoticed; every extra line is a \
         shape the rule asks for being reported as a violation"
    );
}

#[test]
fn a_bound_named_from_a_constant_above_the_test_module_is_still_read() {
    // the shape: a production `pub const ... Duration`, which the test
    // region the walk clips a `src` file to does not contain, standing as
    // the whole value of a bound inside that region
    let source = "pub const MOTION_SLOW: Duration = Duration::from_millis(120);\n#[cfg(test)]\nmod tests {\n    fn t() {\n        assert!(elapsed <= MOTION_SLOW);\n    }\n}\n";
    let found = absolute_span_bounds(&common::test_region(source), source);
    let at: Vec<usize> = found.iter().map(|found| found.number).collect();
    assert_eq!(
        at,
        vec![5],
        "a constant declared where the test region cannot see it still \
         names an absolute, and a bound written with its name is the same \
         bound written with its value"
    );
}

#[test]
fn an_indented_test_module_is_reported_at_the_lines_it_occupies() {
    // the shape: a `#[cfg(test)]` that is not in column one, which is what
    // a test module nested inside another module looks like
    let source = "fn f() {}\nmod outer {\n    #[cfg(test)]\n    mod tests {\n                          let got = rx.recv_timeout(Duration::from_secs(2));\n    }\n}\n";
    let found = absolute_span_bounds(&common::test_region(source), source);
    let at: Vec<usize> = found.iter().map(|found| found.number).collect();
    assert_eq!(
        at,
        vec![5],
        "the wait is on line 5 of the source; a report that names any other \
         line sends its reader to code that does not hold the bound"
    );
}

#[test]
fn the_walk_reaches_test_sources_in_nested_directories() {
    let sources = common::workspace_test_sources();
    for nested in [
        "view-engine/tests/common/mod.rs",
        "view-oracle/src/hang/tests.rs",
    ] {
        let read = sources
            .iter()
            .find(|(name, _)| name == nested)
            .map(|(_, source)| source.trim().len())
            .unwrap_or_default();
        assert!(
            read > 0,
            "the walk read {read} bytes of {nested}, so a test module one \
             directory down -- or one that is a whole file rather than a \
             `#[cfg(test)]` region inside one -- is a place the rule does \
             not look"
        );
    }
}

/// A call that hands a duration to the load-scaled budget, which is what
/// makes the duration a base rather than a bound.
const SCALERS: [&str; 3] = ["host_deadline", "HostBudget", "rpc_deadline"];

/// The blocking waits that spend a wall clock instead of comparing one.
///
/// Named with their opening paren so a field or a differently-spelled
/// method with the same prefix is not read as one.
const BLOCKING_WAITS: [&str; 2] = ["recv_timeout(", "wait_timeout("];

/// One place a measured span is bounded by a wall clock nothing scales.
struct AbsoluteBound {
    number: usize,
    line: String,
}

/// Every absolute bound in `source`, in the order they appear.
///
/// Three shapes count. A comparison between a measured span (per
/// [`MEASURED_SPANS`]) and an unscaled
/// duration, whichever side each sits on -- a floor (`elapsed >= ...`) needs
/// declared grounds for the opposite reason a ceiling does: a stalled host
/// inflates the span, so a floor whose delay has gone *passes*, and nothing
/// in the suite is left to notice. A deadline built as `Instant::now() +
/// <unscaled duration>`, which is the same wall clock with the subtraction
/// moved. And a wall clock handed to a blocking wait (`recv_timeout`,
/// `wait_timeout`), where the comparison happens inside the standard library
/// and the failure it produces on a loaded host is a timed-out receive
/// rather than a failed assertion. All three read through the statement
/// rather than the line, so wrapping hides none of them, and all three
/// resolve a bare constant against `declarations` -- the whole file, not
/// the test region `source` is clipped to, since a test's bound can name a
/// production constant declared above the test module and reading only the
/// region would take it for an identifier of unknown value.
///
/// Which side is the span is settled by what it is called, per
/// [`MEASURED_SPANS`], which is where this rule stops: a bound on a span
/// named outside that list is not read at all.
fn absolute_span_bounds(source: &str, declarations: &str) -> Vec<AbsoluteBound> {
    let lines: Vec<&str> = source.lines().collect();
    let consts = absolute_constants(&statements(declarations));
    let statements = statements(source);
    let mut found = Vec::new();
    let mut push = |at: usize, statement: &Statement| {
        let number = statement.lines.get(at).copied().unwrap_or(1);
        let line = lines
            .get(number.saturating_sub(1))
            .unwrap_or(&"")
            .trim()
            .to_owned();
        found.push(AbsoluteBound { number, line });
    };
    for statement in &statements {
        for (at, span, bound) in comparisons(&statement.text) {
            if names_a_measured_span(span) && is_absolute(bound, &consts) {
                push(at, statement);
            }
        }
        for at in offsets_of(&statement.text, "Instant::now") {
            let rest = &statement.text[at..];
            let Some(plus) = rest.find('+') else {
                continue;
            };
            if is_absolute(first_argument(&rest[plus + 1..]), &consts) {
                push(at + plus, statement);
            }
        }
        for waiter in BLOCKING_WAITS {
            for at in offsets_of(&statement.text, waiter) {
                let open = at + waiter.len() - 1;
                if is_absolute(call_arguments(&statement.text[open..]), &consts) {
                    push(open, statement);
                }
            }
        }
    }
    found.sort_by_key(|bound| bound.number);
    found.dedup_by_key(|bound| bound.number);
    found
}

/// Whether `expr` is a duration that nothing widens with the host's load.
fn is_absolute(expr: &str, consts: &HashSet<String>) -> bool {
    if SCALERS.iter().any(|scaler| expr.contains(scaler)) {
        return false;
    }
    holds_a_duration_literal(expr)
        || is_a_bare_number(expr)
        || identifiers(expr).any(|name| consts.contains(&name))
}

/// What this tree calls a span it measured, which is how the walk tells
/// one side of a comparison from the other.
///
/// A list of names and not a type, because the walk reads text: a span is
/// recognisable only by what it is called. That is this rule's boundary
/// and it is written down rather than left to be discovered -- a floor or
/// a ceiling on a span named anything else is invisible here, and the fix
/// when one appears is to name it out of this list or to add its name to
/// it. `after` is deliberately absent: it is a substring of too many
/// identifiers to tell a span from a sentence. The boundary is pinned from
/// the other side too: `a_span_named_outside_the_list_is_where_this_walk_stops`
/// in [`ESCAPING_SHAPES`] is a bound this list does not reach, and it fails
/// the moment the walk starts reporting it.
const MEASURED_SPANS: &[&str] = &[
    "elapsed",
    "took",
    "waited",
    "observed_for",
    "duration_since",
];

/// Whether `text` names a span some clock produced, per [`MEASURED_SPANS`].
fn names_a_measured_span(text: &str) -> bool {
    MEASURED_SPANS.iter().any(|name| text.contains(name))
}

/// The names of every `const NAME: Duration` in `statements` whose value is
/// an absolute duration.
///
/// One level of resolution, which is all this tree's tests use: a constant
/// declared from another constant is read as whatever its own text says.
fn absolute_constants(statements: &[Statement]) -> HashSet<String> {
    let mut named = HashSet::new();
    for statement in statements {
        let text = statement.text.trim();
        let Some(rest) = text
            .strip_prefix("const ")
            .or(text.strip_prefix("pub const "))
        else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let Some((name, kind)) = name.split_once(':') else {
            continue;
        };
        if kind.contains("Duration") && is_absolute(value, &HashSet::new()) {
            named.insert(name.trim().to_owned());
        }
    }
    named
}

/// Whether `expr` is a plain number, which is what a bound looks like once
/// the span has been converted: `elapsed().as_millis() < 50`.
fn is_a_bare_number(expr: &str) -> bool {
    let digits: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '_')
}

/// Whether `text` builds a `Duration` straight from a number rather than
/// from a named constant.
///
/// `Duration::from_secs(WAIT_WATCHDOG_SECS)` is already derived from the
/// code under test -- it is the very shape this rule asks for -- and only
/// the digits-in-place form is a bound somebody picked by feel.
fn holds_a_duration_literal(text: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find("Duration::from_") {
        rest = &rest[at + "Duration::from_".len()..];
        let Some(open) = rest.find('(') else {
            continue;
        };
        let arg: String = rest[open + 1..]
            .split(')')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace() && *c != ',')
            .collect();
        if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit() || c == '_') {
            return true;
        }
    }
    false
}

/// The identifiers in `text`, which is how a bare constant used as a bound
/// is recognised.
fn identifiers(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(std::borrow::ToOwned::to_owned)
}

/// Every comparison in `text`, as (offset of the operator, the side that
/// may name a measured span, the side that may be its bound).
///
/// All four of `<`, `<=`, `>`, `>=`, because a bound reads the same
/// backwards and an inclusive one is still a bound. An operator with no
/// whitespace before it is a generic's angle bracket, a shift, or an arrow.
fn comparisons(text: &str) -> Vec<(usize, &str, &str)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for (at, c) in text.char_indices() {
        if c != '<' && c != '>' {
            continue;
        }
        let before = at.checked_sub(1).map(|before| bytes[before]);
        let after = bytes.get(at + 1).copied();
        if !before.is_some_and(|before| before.is_ascii_whitespace())
            || matches!(after, Some(b'<' | b'>' | b'-'))
        {
            continue;
        }
        let len = if after == Some(b'=') { 2 } else { 1 };
        let left = &text[..at];
        let right = first_argument(&text[at + len..]);
        found.push((at, left, right));
        found.push((at, right, left));
    }
    found
}

fn offsets_of(text: &str, needle: &str) -> Vec<usize> {
    text.match_indices(needle).map(|(at, _)| at).collect()
}

/// The whole argument list of the call whose opening bracket `text` starts
/// at, without it.
///
/// Every argument rather than the first, because the position a duration is
/// passed in differs per waiter: `recv_timeout` takes it first and
/// `Condvar::wait_timeout` takes it after the guard.
fn call_arguments(text: &str) -> &str {
    let mut depth = 0i32;
    for (at, c) in text.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return &text[1..at];
                }
            }
            _ => {}
        }
    }
    text
}

/// `text` up to the first comma or semicolon outside brackets, which is the
/// whole of the expression a comparison or a `+` was handed.
fn first_argument(text: &str) -> &str {
    let mut depth = 0i32;
    for (at, c) in text.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth == 0 {
                    return &text[..at];
                }
                depth -= 1;
            }
            ',' | ';' if depth == 0 => return &text[..at],
            _ => {}
        }
    }
    text
}

/// One statement of source with the physical line each of its bytes came
/// from, so a wrapped assertion is read whole and still reported where the
/// reader will find it.
#[derive(Default)]
struct Statement {
    text: String,
    lines: Vec<usize>,
}

impl Statement {
    fn push(&mut self, c: char, line: usize) {
        self.text.push(c);
        for _ in 0..c.len_utf8() {
            self.lines.push(line);
        }
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// `source` split into statements, with comments dropped and string
/// contents emptied.
///
/// Emptying strings rather than keeping them is what stops an assertion
/// message that quotes a duration from reading as a bound of its own.
fn statements(source: &str) -> Vec<Statement> {
    let src: Vec<char> = source.chars().collect();
    let mut out: Vec<Statement> = Vec::new();
    let mut cur = Statement::default();
    let mut line = 1usize;
    let mut at = 0usize;
    let peek = |at: usize| src.get(at).copied().unwrap_or('\0');
    while at < src.len() {
        let c = src[at];
        match c {
            '\n' => {
                cur.push(' ', line);
                line += 1;
                at += 1;
            }
            '/' if peek(at + 1) == '/' => {
                while at < src.len() && src[at] != '\n' {
                    at += 1;
                }
            }
            '/' if peek(at + 1) == '*' => {
                let mut depth = 1;
                at += 2;
                while at < src.len() && depth > 0 {
                    if src[at] == '\n' {
                        line += 1;
                    } else if src[at] == '/' && peek(at + 1) == '*' {
                        depth += 1;
                        at += 1;
                    } else if src[at] == '*' && peek(at + 1) == '/' {
                        depth -= 1;
                        at += 1;
                    }
                    at += 1;
                }
            }
            'r' if matches!(peek(at + 1), '"' | '#') => {
                let start = at;
                at += 1;
                let mut hashes = 0;
                while peek(at) == '#' {
                    hashes += 1;
                    at += 1;
                }
                if peek(at) != '"' {
                    // an identifier that merely begins with `r`, or a
                    // lifetime-free `#` that is not a raw string at all
                    at = start;
                    cur.push(c, line);
                    at += 1;
                    continue;
                }
                at += 1;
                let close: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let tail: String = src[at..].iter().collect();
                let end = tail.find(&close).map_or(src.len(), |found| {
                    at + tail[..found].chars().count() + close.chars().count()
                });
                line += src[at..end.min(src.len())]
                    .iter()
                    .filter(|c| **c == '\n')
                    .count();
                at = end;
            }
            '"' => {
                at += 1;
                while at < src.len() {
                    if src[at] == '\\' {
                        // a backslash-newline is how rustfmt continues a
                        // long message, and the newline it hides still moved
                        // every line after it
                        if peek(at + 1) == '\n' {
                            line += 1;
                        }
                        at += 2;
                        continue;
                    }
                    if src[at] == '"' {
                        at += 1;
                        break;
                    }
                    if src[at] == '\n' {
                        line += 1;
                    }
                    at += 1;
                }
            }
            '\'' if peek(at + 1) == '\\' || peek(at + 2) == '\'' => {
                // a character literal; a lifetime has no closing quote and
                // falls through to being pushed like any other token
                while at < src.len() {
                    at += 1;
                    if src[at - 1] == '\\' {
                        at += 1;
                    }
                    if peek(at) == '\'' {
                        at += 1;
                        break;
                    }
                }
            }
            ';' | '{' | '}' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur = Statement::default();
                }
                at += 1;
            }
            _ => {
                cur.push(c, line);
                at += 1;
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_declared(file: &str, found: &AbsoluteBound) -> bool {
    DECLARED_ABSOLUTES
        .iter()
        .any(|declared| declared.file == file && declared.line == found.line)
}

/// `relative` resolved against this crate's `tests` directory, so the walk
/// finds the same sources whatever directory the test binary is run from.
fn crate_relative(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(relative)
}

#[test]
fn every_declared_absolute_is_still_in_the_test_it_names() {
    let sources = common::workspace_test_sources();
    for declared in DECLARED_ABSOLUTES {
        let source = sources
            .iter()
            .find(|(name, _)| name == declared.file)
            .map(|(_, source)| source.as_str())
            .unwrap_or_default();
        let occurrences = source
            .lines()
            .filter(|line| line.trim() == declared.line)
            .count();
        assert_eq!(
            occurrences, 1,
            "{} holds {occurrences} occurrences of {:?}, and DECLARED_ABSOLUTES \
             declares 1, on the grounds that {}. At zero the bound has moved \
             or gone and the exemption is a standing licence nothing claims; \
             above one a second bound is inheriting an exemption nobody \
             granted it -- give each its own entry, or make them tell \
             themselves apart",
            declared.file, declared.line, declared.grounds
        );
    }
}
