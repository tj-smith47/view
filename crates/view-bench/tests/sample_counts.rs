//! A source-text pin on the sample counts scenarios own rather than read
//! off [`Protocol`](view_bench::scenarios::Protocol).
//!
//! A scenario is allowed to fix its own count -- some boundaries cost
//! seconds a sample and the matrix-wide count would price them in hours --
//! but a count below the protocol's is a resolution the gated statistic is
//! read at, and the median of a dozen samples resolves to a quarter of its
//! own value. The rule the walk holds is therefore not "never below the
//! floor" but "below the floor with the statistic named": every
//! scenario-owned count is declared here with what it feeds, why it is not
//! the protocol's, and -- when what it feeds gates as a median -- the test
//! that pins the resolution it buys.
//!
//! The walk reads each scenario source down to its `#[cfg(test)]`
//! boundary, since a count declared inside a test module feeds a test and
//! not a bench row.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use view_bench::scenarios::Protocol;

/// What the statistics a count feeds do at the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feeds {
    /// At least one of them is a median the gate reads on a shared class,
    /// where the count is the estimator's resolution and nothing else
    /// cancels the host.
    GatedMedian,
    /// Tails only, or nothing the gate reads: a shared class never gates a
    /// tail, and a controlled one compares it against its own recording.
    NoGatedMedian,
}

/// One scenario-owned sample count, as the walk finds it.
struct DeclaredCount {
    /// Source file under `src/scenarios/`, as the walk names it.
    file: &'static str,
    /// The constant's name.
    name: &'static str,
    /// Its value exactly as the source writes it, so a count that moves
    /// without its grounds being revisited parts from this entry.
    value: &'static str,
    /// The statistics it feeds, for the reader who finds the entry before
    /// finding the scenario.
    statistics: &'static str,
    /// What the gate does with those statistics.
    feeds: Feeds,
    /// Why the count is not the protocol's.
    grounds: &'static str,
    /// For a [`Feeds::GatedMedian`] count below the protocol floor, the
    /// test in the same file that pins what the count resolves to; empty
    /// otherwise.
    resolution_pin: &'static str,
}

const DECLARED_COUNTS: &[DeclaredCount] = &[
    DeclaredCount {
        file: "picker.rs",
        name: "SCAN_SAMPLES",
        value: "100",
        statistics: "first_page_p50_ms, first_page_p99_ms",
        feeds: Feeds::GatedMedian,
        grounds: "one sample is a picker open against a million-file walk, and the \
                  slowest hosted class bounds an open at 5.2 s, which prices the \
                  protocol's own count above four hours",
        resolution_pin: "the_scan_count_resolves_its_median_where_a_dozen_opens_did_not",
    },
    DeclaredCount {
        file: "picker.rs",
        name: "SCAN_WARMUP",
        value: "SCAN_SAMPLES / 10",
        statistics: "none; warmup is excluded from every statistic",
        feeds: Feeds::NoGatedMedian,
        grounds: "it is the share the protocol keeps between its own warmup and \
                  samples, so it follows the measured count rather than being \
                  sized twice",
        resolution_pin: "",
    },
    DeclaredCount {
        file: "supervision.rs",
        name: "SAMPLES",
        value: "3",
        statistics: "wedge_detect_p99_ms, restart_rehydrate_p99_ms",
        feeds: Feeds::NoGatedMedian,
        grounds: "a wedge sample cannot return before the heartbeat's own ceiling \
                  has elapsed, about 11 s each, and both statistics the row \
                  publishes are tails rather than medians",
        resolution_pin: "",
    },
];

/// Names a scenario-owned count carries; a declaration whose name holds
/// one of these is a workload size and is walked.
const COUNT_MARKERS: &[&str] = &["SAMPLES", "TRIALS", "WARMUP"];

fn scenarios_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("scenarios")
}

/// Every `*.rs` under `src/scenarios/`, including its subdirectories.
fn scenario_sources(dir: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the scenarios directory must be readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            found.extend(scenario_sources(&path));
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("a source file name")
                .to_string();
            let body = std::fs::read_to_string(&path).expect("a readable source file");
            found.push((name, body));
        }
    }
    found
}

/// The `const <NAME>: usize = <value>;` declarations a file carries above
/// its test module, whose name marks them a workload size.
fn declared_counts(source: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with("#[cfg(test)]") {
            break;
        }
        let Some(rest) = line
            .strip_prefix("const ")
            .or_else(|| line.strip_prefix("pub const "))
        else {
            continue;
        };
        let Some((name, value)) = rest.split_once(": usize = ") else {
            continue;
        };
        if !COUNT_MARKERS.iter().any(|marker| name.contains(marker)) {
            continue;
        }
        found.push((
            name.to_string(),
            value.trim_end_matches(';').trim().to_string(),
        ));
    }
    found
}

#[test]
fn every_scenario_owned_sample_count_is_declared_with_what_it_feeds() {
    let mut undeclared = Vec::new();
    for (file, source) in scenario_sources(&scenarios_dir()) {
        for (name, value) in declared_counts(&source) {
            let matched = DECLARED_COUNTS
                .iter()
                .any(|entry| entry.file == file && entry.name == name && entry.value == value);
            if !matched {
                undeclared.push(format!("{file}: {name} = {value}"));
            }
        }
    }
    assert!(
        undeclared.is_empty(),
        "these scenario-owned counts fix the resolution a bench statistic is read \
         at, and nothing here says which statistic or why the protocol's own count \
         was not taken:\n  {}\nAdd an entry naming what it feeds and the grounds; a \
         count that gates as a median also names the test pinning what it resolves \
         to.",
        undeclared.join("\n  ")
    );
}

#[test]
fn no_declared_sample_count_outlives_the_constant_it_describes() {
    let sources = scenario_sources(&scenarios_dir());
    for entry in DECLARED_COUNTS {
        let found = sources.iter().any(|(file, source)| {
            *file == entry.file
                && declared_counts(source)
                    .iter()
                    .any(|(name, value)| name == entry.name && value == entry.value)
        });
        assert!(
            found,
            "{}'s {} = {} is declared here and no longer in the source; a count that \
             moved was sized again, so its grounds ({}) are what needs revisiting, \
             not this entry alone",
            entry.file, entry.name, entry.value, entry.grounds
        );
    }
}

#[test]
fn a_gated_median_below_the_protocol_floor_states_its_exemption() {
    let floor = Protocol::default().samples;
    let sources = scenario_sources(&scenarios_dir());
    for entry in DECLARED_COUNTS {
        let above_floor = entry
            .value
            .parse::<usize>()
            .is_ok_and(|value| value >= floor);
        if entry.feeds != Feeds::GatedMedian || above_floor {
            continue;
        }
        assert!(
            !entry.grounds.is_empty(),
            "{}'s {} gates {} at {} samples against the protocol's {floor} and states \
             no grounds for the shortfall",
            entry.file,
            entry.name,
            entry.statistics,
            entry.value
        );
        let source = sources
            .iter()
            .find(|(file, _)| *file == entry.file)
            .map(|(_, source)| source)
            .expect("a declared count's own source file");
        assert!(
            !entry.resolution_pin.is_empty() && source.contains(entry.resolution_pin),
            "{}'s {} gates {} at {} samples against the protocol's {floor}, so what \
             that count resolves to is a property of the row rather than of the host, \
             and needs a test in the same file to pin it; {:?} is not there",
            entry.file,
            entry.name,
            entry.statistics,
            entry.value,
            entry.resolution_pin
        );
    }
}
