//! Source-text pins on how the workspace's integration tests are allowed to
//! assert time.
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
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

/// This file's own name, which the walk below skips.
const SELF_SOURCE: &str = "timing_bounds.rs";

/// Where view-tui defines the constant `common::PROBE_DEADLINE` copies,
/// relative to this crate's own `tests` directory.
const TIERS_SOURCE: &str = "../../view-tui/src/tiers.rs";

/// An `elapsed < <literal duration>` assertion that is allowed to stay one.
///
/// Every entry bounds a measured span against a timing that belongs to
/// something outside this workspace, which is a real discriminator rather
/// than a guess at how fast the host is: a load-scaled budget would widen
/// it past the very behaviour it exists to tell apart.
struct DeclaredAbsolute {
    /// `<crate>/tests/<file>.rs`, as the walk below names what it finds.
    file: &'static str,
    line: &'static str,
    /// The behaviour the bound sits below, for the reader who finds the
    /// entry before finding the test.
    discriminates: &'static str,
}

const DECLARED_ABSOLUTES: &[DeclaredAbsolute] = &[
    DeclaredAbsolute {
        file: "view-oracle/tests/osc52_user_provider_paste.rs",
        line: "elapsed < Duration::from_secs(2),",
        discriminates: "nvim's own OSC 52 provider waiting out its first vim.wait(1000)",
    },
    DeclaredAbsolute {
        file: "view-oracle/tests/osc52_user_provider_paste.rs",
        line: "elapsed < Duration::from_secs(3),",
        discriminates: "the same provider's wait, for the empty-clipboard answer",
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
            // this file quotes the very shape it forbids, in
            // DECLARED_ABSOLUTES
            continue;
        }
        for (number, line) in source.lines().enumerate() {
            let line = line.trim();
            if !is_absolute_span_bound(line) || is_declared(&name, line) {
                continue;
            }
            undeclared.push(format!("{name}:{}: {line}", number + 1));
        }
    }
    assert!(
        undeclared.is_empty(),
        "these assertions bound a measured span with a hand-picked absolute, \
         which fails on a loaded host without saying anything about view:\n  \
         {}\nDerive the bound from the constants of the code under test and \
         let view_test_support::HostBudget scale the host's share (see \
         common::startup_budget here, or view-engine's own rpc_deadline), or \
         -- if the bound really does sit below a known competing behaviour -- \
         add it to DECLARED_ABSOLUTES with the behaviour it discriminates \
         against",
        undeclared.join("\n  ")
    );
}

/// Every `crates/*/tests/*.rs` in the workspace, as
/// (`<crate>/tests/<file>.rs`, contents).
///
/// Integration tests only: a `#[cfg(test)]` block inside `src/` belongs to
/// the crate that owns the file, and a walk that reached into those would
/// be this crate asserting over source its own gate does not own.
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
        let tests = member.join("tests");
        let Ok(entries) = std::fs::read_dir(&tests) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .collect();
        paths.sort();
        for path in paths {
            let name = path
                .strip_prefix(&crates)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let source = std::fs::read_to_string(&path).expect("a tests/ source must be readable");
            found.push((name, source));
        }
    }
    assert!(
        found.len() > 20,
        "the walk found only {} test sources, so it is not looking where the \
         workspace keeps them and would pass by finding nothing",
        found.len()
    );
    found
}

/// A call that hands a literal duration to the load-scaled budget, which is
/// what makes the literal a base rather than a bound.
const SCALERS: [&str; 3] = ["host_deadline", "HostBudget", "rpc_deadline"];

/// Whether `line` compares a measured span against a literal duration that
/// nothing scales.
///
/// Deliberately narrow: a `wait_for(.., Duration::from_secs(5))` is a
/// ceiling on how long a test is willing to wait for something to happen,
/// not a claim that it happened quickly, and only the latter turns host
/// load into a failure.
fn is_absolute_span_bound(line: &str) -> bool {
    let Some((left, right)) = line.split_once('<') else {
        return false;
    };
    (left.contains("elapsed") || left.contains("took"))
        && right.contains("Duration::from_")
        && !SCALERS.iter().any(|scaler| right.contains(scaler))
}

fn is_declared(file: &str, line: &str) -> bool {
    DECLARED_ABSOLUTES
        .iter()
        .any(|declared| declared.file == file && declared.line == line)
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
        assert!(
            source.lines().any(|line| line.trim() == declared.line),
            "DECLARED_ABSOLUTES still exempts {:?} in {}, on the grounds that \
             it discriminates against {}, but that file no longer contains \
             the line. Drop the entry rather than leaving a standing \
             exemption nothing claims",
            declared.line,
            declared.file,
            declared.discriminates
        );
    }
}
