//! A source-text pin on where the workspace's tests are allowed to put a
//! scratch directory.
//!
//! A test that assembles its own path under [`std::env::temp_dir`] owns the
//! removal of whatever it creates there, and the removal it writes is a
//! trailing statement -- which the failing assertion above it skips, and
//! which the run never reaches at all when the fixture is handed back from
//! a helper. Neither is visible in a green run: what is visible is a temp
//! root that grows by one directory per test per run until the host has no
//! space left, which is where this rule comes from (117 directories from a
//! single run, on the macOS validation host).
//!
//! [`view_test_support::ScratchDir`] is the guard that cannot be skipped:
//! its `Drop` runs on every exit path, panic included. So the rule is that
//! test code names a scratch path through it and never through
//! `temp_dir().join(...)`.
//!
//! Housed beside `timing_bounds.rs` and reading the population through the
//! same [`common::workspace_test_sources`] walk, for the same reason that
//! one is here: the rule is one rule, and a copy of it per crate is a rule
//! that drifts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

/// This file's own name, which the walk below skips: it quotes the shape it
/// forbids, in the exemptions and in the fixture.
const SELF_SOURCE: &str = "scratch_dirs.rs";

/// Where the guard itself lives, which necessarily assembles the path the
/// rest of the workspace is refused.
const GUARD_SOURCE: &str = "view-test-support/src/lib.rs";

/// A `temp_dir()` join that is allowed to stay one.
///
/// Keyed by the statement's text, which must occur exactly once in the
/// file, on the same terms `timing_bounds.rs`'s own exemptions are: a
/// second copy of the line cannot inherit an exemption nobody granted it.
struct DeclaredJoin {
    /// The path under `crates/`, as the walk below names what it finds.
    file: &'static str,
    /// The line's own text, trimmed, which must appear exactly once.
    line: &'static str,
    /// Why this one creates nothing a guard would have to remove.
    grounds: &'static str,
}

const DECLARED_JOINS: &[DeclaredJoin] = &[DeclaredJoin {
    file: "view-ai/src/config.rs",
    line: r#"let missing = std::env::temp_dir().join("view-ai-config-does-not-exist.toml");"#,
    grounds: "it names a path that must NOT exist, and a guard that created \
              a directory there would be the thing that broke it",
}];

#[test]
fn no_test_assembles_a_scratch_path_outside_the_guard() {
    let mut undeclared = Vec::new();
    for (name, source) in common::workspace_test_sources() {
        if name.ends_with(SELF_SOURCE) || name == GUARD_SOURCE {
            continue;
        }
        for (number, line) in joins_under_the_temp_root(&source) {
            if is_declared(&name, &line) {
                continue;
            }
            undeclared.push(format!("{name}:{number}: {line}"));
        }
    }
    assert!(
        undeclared.is_empty(),
        "these tests assemble a scratch path under the system temp root by \
         hand, so whatever they create there outlives a failing assertion \
         and outlives the run:\n  {}\nTake the path from \
         view_test_support::ScratchDir instead -- it removes the tree on \
         every exit path, panic included -- or, if the site creates nothing \
         at all, add it to DECLARED_JOINS with the grounds that say so",
        undeclared.join("\n  ")
    );
}

/// The shapes the walk must see, each wrong on purpose: a rule proved only
/// by a population that already satisfies it cannot tell "nothing is wrong"
/// from "nothing is being read".
const ESCAPING_SHAPES: &str = r#"
#[cfg(test)]
mod tests {
    fn one_line() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("view-thing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rustfmt_wrapped() -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("view-other-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn imported_directly() -> PathBuf {
        temp_dir().join("view-third")
    }

    fn these_are_the_shapes_the_rule_asks_for() {
        let cwd = std::env::temp_dir();
        let launch = AgentLaunch::new("agent", std::env::temp_dir());
        let real = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let guarded = ScratchDir::new("label").unwrap().join("file.txt");
    }
}
"#;

#[test]
fn the_walk_sees_a_join_however_it_is_written() {
    let at: Vec<usize> = joins_under_the_temp_root(ESCAPING_SHAPES)
        .into_iter()
        .map(|(number, _)| number)
        .collect();
    assert_eq!(
        at,
        vec![5, 11, 18],
        "the walk read {at:?} of the fixture. Every line it missed is a leak \
         the population can carry unnoticed; every extra line is a site the \
         rule does not ask about being reported as a violation"
    );
}

/// Every place in `source` that hangs a name off the system temp root, as
/// (line number, the statement's text on one line).
///
/// Statements rather than lines, because rustfmt puts the `.join(` on the
/// line after the call as soon as the name is long enough, and a
/// line-at-a-time reader sees neither half. Reported at the line the call
/// itself is on, which is where a reader will find it.
fn joins_under_the_temp_root(source: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if !line.contains("temp_dir()") {
            continue;
        }
        // the statement, as far as the next `;` or the end of the file:
        // enough to see a `.join(` rustfmt moved to its own line
        let mut statement = String::new();
        for later in &lines[index..] {
            statement.push_str(later.trim());
            if later.contains(';') {
                break;
            }
        }
        let hangs_a_name = statement.contains("temp_dir().join(");
        if hangs_a_name {
            found.push((index + 1, line.trim().to_string()));
        }
    }
    found
}

/// Whether `line` in `file` is one of [`DECLARED_JOINS`], failing loudly
/// when an entry names a line the file no longer holds exactly once -- a
/// stale exemption is an exemption granted to whatever moved into its
/// place.
fn is_declared(file: &str, line: &str) -> bool {
    DECLARED_JOINS.iter().any(|declared| {
        if declared.file != file || declared.line != line {
            return false;
        }
        assert!(
            !declared.grounds.is_empty(),
            "{file}: an exemption with no grounds is not one"
        );
        true
    })
}

#[test]
fn every_exemption_still_names_a_line_its_file_holds_exactly_once() {
    let sources = common::workspace_test_sources();
    for declared in DECLARED_JOINS {
        let source = sources
            .iter()
            .find(|(name, _)| name == declared.file)
            .map(|(_, source)| source.as_str())
            .unwrap_or_else(|| panic!("{}: the walk does not read this file", declared.file));
        let hits = source
            .lines()
            .filter(|line| line.trim() == declared.line)
            .count();
        assert_eq!(
            hits, 1,
            "{}: the exempted line occurs {hits} times, not once -- an \
             exemption that names nothing exempts nothing, and one that \
             names two lines was granted to only one of them:\n  {}",
            declared.file, declared.line
        );
    }
}
