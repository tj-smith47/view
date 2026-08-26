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
const HARNESS_SHAPES: [&str; 6] = [
    "cargo test",
    "cargo bench",
    "--bin bench",
    "--bin oracle",
    "scripts/mutate.sh",
    "scripts/acceptance/",
];

/// A built harness invoked by path, which runs the same clock reads with
/// no cargo anywhere on the line.
///
/// Matched as a whole path component rather than as a substring: `/bench`
/// also opens `crates/view-tui/benches/paint_frame.rs`, and a line naming a
/// bench *source file* would otherwise be reported as an unheld harness.
/// The binary's name ends its path, so the character after it -- if there
/// is one -- is never one a file name continues with.
const BINARY_SHAPES: [&str; 2] = ["/bench", "/oracle"];

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
///
/// One fixture per shape, each carrying the line the walk must report,
/// rather than one fixture and a vector of offsets: a walk that stops
/// seeing one spelling is then red on that spelling by name, and editing
/// any shape cannot renumber the others. Every entry is `(what the shape
/// is, the source, the line the walk must report)`.
const ESCAPING_SHAPES: &[(&str, &str, usize)] = &[
    (
        "a bare task command",
        r#"
  bare:
    cmd: cargo test --workspace
"#,
        3,
    ),
    (
        "one held and one unheld command in the same task",
        r#"
  micro:
    cmds:
      - cargo bench -p view-core --bench grid_apply
      - bash scripts/hold-awake.sh cargo bench -p view-core --bench update_key
"#,
        4,
    ),
    (
        "a harness run through cargo run",
        r#"
  harness:
    cmd: cargo run -p view-harness --bin oracle -- compat
"#,
        3,
    ),
    (
        "a workflow step",
        r#"
  workflow_step:
        run: cargo test -p view-oracle --test clipboard_roundtrip
"#,
        3,
    ),
    (
        "the bench binary invoked by path",
        r#"
  direct_binary:
        run: ./target/release/bench --all --class dev-linux
"#,
        3,
    ),
    (
        "the oracle binary invoked by path",
        r#"
  direct_oracle:
        run: ./target/release/oracle compat
"#,
        3,
    ),
];

/// The spellings the rule does not ask about, which the walk must leave
/// alone: prose naming a harness, a build that measures nothing, a command
/// already under the seam, an acceptance script that takes the assertion in
/// its own shell, and a path that merely opens with a harness binary's
/// name. A rule that flagged these would be unstatable in its own Taskfile.
const HELD_SHAPES: &str = r#"
  # a comment naming cargo test is prose, not a command
  these_are_the_shapes_the_rule_asks_for:
    desc: run cargo test the way the docs say (cargo bench too)
    cmds:
      - cargo build --release -p view
      - cargo build --release -p view-harness --bin bench
      - bash scripts/hold-awake.sh cargo test --workspace
      - bash scripts/acceptance/visual-sweep.sh
      - task bench -- --all --record
  bench_source_file:
        run: sed -n '1,40p' crates/view-tui/benches/paint_frame.rs
  oracle_source_dir:
        run: ls crates/view-oracle/tests/oracle_cases
"#;

#[test]
fn the_walk_sees_an_unheld_harness_however_it_is_written() {
    for (shape, source, at) in ESCAPING_SHAPES {
        let found: Vec<usize> = unheld_harnesses(source)
            .into_iter()
            .map(|(number, _)| number)
            .collect();
        assert_eq!(
            found,
            vec![*at],
            "the walk read {found:?} of the fixture for {shape}, which is at \
             line {at}. A shape it misses is a harness that can time itself \
             against a stopped clock unnoticed; a line it adds is a command \
             the rule does not ask about being reported as a violation"
        );
    }
}

#[test]
fn the_walk_leaves_the_commands_the_rule_does_not_ask_about_alone() {
    let found = unheld_harnesses(HELD_SHAPES);
    assert!(
        found.is_empty(),
        "the walk reported {found:?}, so it flags a line the rule does not \
         ask about -- prose, a build, a command already under the seam, or \
         a path that merely opens with a harness binary's name"
    );
}

/// Whether `line` runs the binary `shape` names, rather than merely
/// holding its name inside a longer path component.
fn names_the_binary(line: &str, shape: &str) -> bool {
    let mut rest = line;
    while let Some(at) = rest.find(shape) {
        rest = &rest[at + shape.len()..];
        let next = rest.chars().next();
        if !next.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
            return true;
        }
    }
    false
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
        if !HARNESS_SHAPES.iter().any(|shape| line.contains(shape))
            && !BINARY_SHAPES
                .iter()
                .any(|shape| names_the_binary(line, shape))
        {
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
