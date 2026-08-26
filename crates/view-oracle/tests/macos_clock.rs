//! A source-text pin on which commands are allowed to time anything.
//!
//! macOS takes unattended maintenance sleeps, and a Mach monotonic clock
//! does not advance across one: measured on this repo's own macOS host,
//! view's log advanced 11s over 313s of wall. Every harness here that reads
//! a clock and asserts a duration -- the bench matrix, the micro-benches,
//! the heartbeat campaign, the oracle and compat legs, the acceptance
//! scripts, the test suite's own live legs -- reports numbers nothing
//! produced if it runs across one.
//!
//! `scripts/hold-awake.sh` is the one place the power assertion is taken.
//! The rule is that every timed harness comes through it, and this walks
//! `Taskfile.yml` and `.github/workflows/` to say so: an acceptance script
//! by way of `scripts/acceptance/artifacts.sh`, which sources the same
//! file, and everything else by naming it outright.
//!
//! Housed beside `scratch_dirs.rs` and `timing_bounds.rs` because it is the
//! same kind of rule: one the next member has to trip over mechanically
//! rather than be told about.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// The seam itself.
const SEAM: &str = "scripts/hold-awake.sh";

/// Where the acceptance scripts take the same assertion, in their own
/// shell, by sourcing [`SEAM`].
const ACCEPTANCE_SEAM: &str = "scripts/acceptance/artifacts.sh";

/// What makes a command a timed harness: it runs something that reads a
/// clock and asserts a duration.
///
/// Substrings of the command rather than a parse, because that is what a
/// reader adding a line will recognize. A `cargo build` is not here: a
/// build measures nothing, and holding one awake would only widen the rule
/// past what it can justify.
const HARNESS_SHAPES: [&str; 8] = [
    "cargo test",
    "cargo bench",
    "--bin bench",
    "--bin oracle",
    // a built harness invoked by path runs the same clock reads without
    // cargo anywhere on the line
    "/bench",
    "/oracle",
    "scripts/mutate.sh",
    "scripts/acceptance/",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("this crate sits two levels under the workspace root")
        .to_owned()
}

#[test]
fn every_timed_harness_runs_under_the_power_assertion() {
    let root = repo_root();
    let mut sources = vec![(
        "Taskfile.yml".to_string(),
        std::fs::read_to_string(root.join("Taskfile.yml")).expect("the Taskfile must be readable"),
    )];
    let mut workflows: Vec<PathBuf> = std::fs::read_dir(root.join(".github/workflows"))
        .expect("the workflow directory must be readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .collect();
    workflows.sort();
    assert!(
        !workflows.is_empty(),
        "the walk found no workflows, so it is not looking where they live \
         and would pass by finding nothing"
    );
    for workflow in workflows {
        let name = format!(
            ".github/workflows/{}",
            workflow.file_name().unwrap().to_string_lossy()
        );
        sources.push((
            name,
            std::fs::read_to_string(&workflow).expect("a workflow must be readable"),
        ));
    }

    let mut unheld = Vec::new();
    for (name, text) in &sources {
        for (number, line) in unheld_harnesses(text) {
            unheld.push(format!("{name}:{number}: {line}"));
        }
    }
    assert!(
        unheld.is_empty(),
        "these commands run a harness that reads a clock without the power \
         assertion that keeps macOS from sleeping through it, so on that \
         host they report durations nothing produced:\n  {}\nPut `bash \
         {SEAM}` in front of the command -- it is a no-op everywhere else \
         -- or, in a workflow, call the `task` target that already does",
        unheld.join("\n  ")
    );
}

#[test]
fn the_seam_still_takes_the_assertion() {
    let root = repo_root();
    let seam = std::fs::read_to_string(root.join(SEAM)).expect("the seam must be readable");
    assert!(
        seam.contains("caffeinate -dims -w $$"),
        "{SEAM} no longer holds a power assertion for its own shell, so \
         every command routed through it is unheld and nothing says so"
    );
    assert!(
        seam.contains("exec \"$@\""),
        "{SEAM} no longer becomes the command it is given, so every Taskfile \
         line that fronts a harness with it runs nothing"
    );
    let acceptance =
        std::fs::read_to_string(root.join(ACCEPTANCE_SEAM)).expect("artifacts.sh must be readable");
    assert!(
        acceptance.contains(SEAM),
        "{ACCEPTANCE_SEAM} no longer sources {SEAM}, so the acceptance \
         scripts -- which the walk above lets through on the grounds that it \
         does -- are running unheld"
    );
    for (name, text) in acceptance_scripts(&root) {
        if name == ACCEPTANCE_SEAM {
            continue;
        }
        assert!(
            text.contains("artifacts.sh"),
            "{name} does not source {ACCEPTANCE_SEAM}, so it takes no power \
             assertion of its own -- and the walk above lets every command \
             named scripts/acceptance/ through on the grounds that it does"
        );
    }
}

/// Every acceptance script, as (path under the repo root, its contents).
fn acceptance_scripts(root: &Path) -> Vec<(String, String)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(root.join("scripts/acceptance"))
        .expect("the acceptance directory must be readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "sh"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "the walk found no acceptance scripts, so it is not looking where \
         they live and would pass by finding nothing"
    );
    paths
        .into_iter()
        .map(|path| {
            let name = format!(
                "scripts/acceptance/{}",
                path.file_name().unwrap().to_string_lossy()
            );
            let text =
                std::fs::read_to_string(&path).expect("an acceptance script must be readable");
            (name, text)
        })
        .collect()
}

/// The shapes the walk must see, each wrong on purpose: a rule proved only
/// by a population that already satisfies it cannot tell "nothing is wrong"
/// from "nothing is being read".
const ESCAPING_SHAPES: &str = r#"
  bare:
    cmd: cargo test --workspace
  micro:
    cmds:
      - cargo bench -p view-core --bench grid_apply
      - bash scripts/hold-awake.sh cargo bench -p view-core --bench update_key
  harness:
    cmd: cargo run -p view-harness --bin oracle -- compat
  # a comment naming cargo test is prose, not a command
  workflow_step:
        run: cargo test -p view-oracle --test clipboard_roundtrip
  direct_binary:
        run: ./target/release/bench --all --class dev-linux
  direct_oracle:
        run: ./target/release/oracle compat
  these_are_the_shapes_the_rule_asks_for:
    desc: run cargo test the way the docs say (cargo bench too)
    cmds:
      - cargo build --release -p view
      - cargo build --release -p view-harness --bin bench
      - bash scripts/hold-awake.sh cargo test --workspace
      - bash scripts/acceptance/visual-sweep.sh
      - task bench -- --all --record
"#;

#[test]
fn the_walk_sees_an_unheld_harness_however_it_is_written() {
    let at: Vec<usize> = unheld_harnesses(ESCAPING_SHAPES)
        .into_iter()
        .map(|(number, _)| number)
        .collect();
    assert_eq!(
        at,
        vec![3, 6, 9, 12, 14, 16],
        "the walk read {at:?} of the fixture. Every line it missed is a \
         harness that can time itself against a stopped clock unnoticed; \
         every extra line is a command the rule does not ask about being \
         reported as a violation"
    );
}

/// Every command line in `text` that runs a timed harness without the
/// power assertion, as (line number, the command's text trimmed).
///
/// Comments and `desc:` lines are prose about commands rather than
/// commands, and both name harnesses freely; a walk that read them would
/// make the rule unstatable in its own documentation.
fn unheld_harnesses(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') || line.starts_with("desc:") {
            continue;
        }
        if !HARNESS_SHAPES.iter().any(|shape| line.contains(shape)) {
            continue;
        }
        // `cargo build --release -p view-harness --bin bench` builds the
        // harness rather than running it: it measures nothing, and the
        // `--bin bench` in it is the same substring the run would carry
        if line.contains("cargo build") {
            continue;
        }
        // an acceptance script takes the same assertion for itself, in its
        // own shell, by sourcing artifacts.sh -- which the test above holds
        // to sourcing the seam
        if line.contains("scripts/acceptance/") || line.contains(SEAM) {
            continue;
        }
        found.push((index + 1, line.to_string()));
    }
    found
}
