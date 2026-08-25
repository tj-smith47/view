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
        file: "view-engine/tests/shutdown.rs",
        line: "let deadline = std::time::Instant::now() + GRACEFUL_EXIT_LIVENESS_BOUND;",
        grounds: "a child that has not run at all in a minute is not a \
                  descheduled child, which is the whole of what the bound claims",
    },
    DeclaredAbsolute {
        file: "view-oracle/src/reference.rs",
        line: "started.elapsed() < QUIESCE_DEADLINE,",
        grounds: "it is the deadline quiesce itself was handed, which the \
                  assertion exists to prove was not exhausted",
    },
];

#[test]
fn the_probe_deadline_copy_still_matches_view_tuis_own() {
    let source = std::fs::read_to_string(crate_relative(TIERS_SOURCE))
        .expect("view-tui's tiers.rs must be readable from this crate");
    let expected = format!(
        "pub const PROBE_DEADLINE: Duration = Duration::from_millis({});",
        common::PROBE_DEADLINE.as_millis()
    );
    assert!(
        source.contains(&expected),
        "common::PROBE_DEADLINE reads {:?}, which {TIERS_SOURCE} no longer \
         declares. This crate may not depend on view-tui, so the copy is \
         held here instead of by the compiler: update it, and with it every \
         startup budget derived from it, to whatever the definition now \
         says",
        common::PROBE_DEADLINE
    );
}

#[test]
fn no_timing_test_bounds_a_measured_span_with_an_undeclared_absolute() {
    let mut undeclared = Vec::new();
    for (name, source) in workspace_test_sources() {
        if name.ends_with(SELF_SOURCE) {
            // this file quotes the very shapes it forbids, in
            // DECLARED_ABSOLUTES and in the fixture below
            continue;
        }
        for found in absolute_span_bounds(&source) {
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
    fn converting_the_span_hides_the_type_not_the_clock() {
        assert!(start.elapsed().as_millis() < 50, "took {elapsed:?}");
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
    }
}
"#;

#[test]
fn the_walk_sees_every_shape_a_line_at_a_time_reader_missed() {
    let found: Vec<usize> = absolute_span_bounds(ESCAPING_SHAPES)
        .iter()
        .map(|found| found.number)
        .collect();
    // the six deliberately wrong lines: the named constant, the
    // rustfmt-wrapped comparison, the deadline built from `now`, the
    // inclusive bound, the same bound written backwards, and the one whose
    // span is converted to a number first
    assert_eq!(
        found,
        vec![9, 16, 23, 29, 34, 39],
        "the walk read {found:?} of the fixture. Every line it missed is a \
         shape the population can carry unnoticed; every extra line is a \
         shape the rule asks for being reported as a violation"
    );
}

#[test]
fn the_walk_reaches_test_sources_in_nested_directories() {
    let sources = workspace_test_sources();
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

/// Every source in the workspace that holds test code, as
/// (path under `crates/`, the test-code part of its contents).
///
/// A `crates/*/tests/**/*.rs` file is test code outright. A `src` file is
/// test code only from its first `#[cfg(test)]` onwards -- the convention
/// this tree follows without exception, and the conservative reading either
/// way: the walk can miss a test written above the module, and can never
/// flag a production line for comparing an elapsed time to a literal, which
/// is a thing production code is entitled to do.
fn workspace_test_sources() -> Vec<(String, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate sits inside the workspace's crates directory")
        .to_owned();
    let mut found = Vec::new();
    let mut members: Vec<PathBuf> = std::fs::read_dir(&crates)
        .expect("the workspace's crates directory must be readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    members.sort();
    for member in members {
        for source in rust_sources(&member.join("tests")) {
            found.push(named(&crates, &source, false));
        }
        for source in rust_sources(&member.join("src")) {
            let whole_file_is_a_test_module = is_test_module_file(&source);
            found.push(named(&crates, &source, !whole_file_is_a_test_module));
        }
    }
    assert!(
        found.len() > 100,
        "the walk found only {} sources, so it is not looking where the \
         workspace keeps them and would pass by finding nothing",
        found.len()
    );
    found
}

/// Whether a `src` file is a test module outright rather than a source
/// file with one at the bottom.
///
/// `src/thing/tests.rs` is the other half of `#[cfg(test)] mod tests;` and
/// carries no attribute of its own, so reading it from a `#[cfg(test)]`
/// onwards reads none of it.
fn is_test_module_file(source: &Path) -> bool {
    source.file_stem().is_some_and(|stem| stem == "tests")
        || source
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|dir| dir == "tests")
}

/// Every `.rs` file at or under `dir`, sorted, or nothing where `dir` does
/// not exist.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut nested: Vec<PathBuf> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            nested.push(path);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            paths.push(path);
        }
    }
    nested.sort();
    for dir in nested {
        paths.extend(rust_sources(&dir));
    }
    paths.sort();
    paths
}

/// `source` as (path under `crates`, contents), keeping only the part after
/// the first `#[cfg(test)]` when `test_region_only`. The elided prefix is
/// replaced by blank lines so reported line numbers stay real.
fn named(crates: &Path, source: &Path, test_region_only: bool) -> (String, String) {
    let name = source
        .strip_prefix(crates)
        .unwrap_or(source)
        .to_string_lossy()
        .replace('\\', "/");
    let text = std::fs::read_to_string(source).expect("a source file must be readable");
    if !test_region_only {
        return (name, text);
    }
    let Some(at) = text.find("#[cfg(test)]") else {
        return (name, String::new());
    };
    let skipped = text[..at].lines().count();
    (name, "\n".repeat(skipped) + &text[at..])
}

/// A call that hands a duration to the load-scaled budget, which is what
/// makes the duration a base rather than a bound.
const SCALERS: [&str; 3] = ["host_deadline", "HostBudget", "rpc_deadline"];

/// One place a measured span is bounded by a wall clock nothing scales.
struct AbsoluteBound {
    number: usize,
    line: String,
}

/// Every absolute bound in `source`, in the order they appear.
///
/// Two shapes count. A comparison whose left side names a measured span
/// (`elapsed`, `took`) and whose right side is an unscaled duration, and a
/// deadline built as `Instant::now() + <unscaled duration>`, which is the
/// same wall clock with the subtraction moved. Both read through the
/// statement rather than the line, so wrapping hides neither, and both
/// resolve a bare constant against the file's own `const` declarations, so
/// naming the literal does not launder it.
fn absolute_span_bounds(source: &str) -> Vec<AbsoluteBound> {
    let lines: Vec<&str> = source.lines().collect();
    let statements = statements(source);
    let consts = absolute_constants(&statements);
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
            if (span.contains("elapsed") || span.contains("took")) && is_absolute(bound, &consts) {
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
        found.push(if c == '<' {
            (at, left, right)
        } else {
            (at, right, left)
        });
    }
    found
}

fn offsets_of(text: &str, needle: &str) -> Vec<usize> {
    text.match_indices(needle).map(|(at, _)| at).collect()
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
    let sources = workspace_test_sources();
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
