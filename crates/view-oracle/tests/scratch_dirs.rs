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
//! test code names a scratch path through it and never reaches
//! `std::env::temp_dir()` at all -- not to extend it, not to bind it to a
//! name the next statement can push onto (the spelling the leak was
//! written in), and not to hand it onward as an argument, a struct field
//! or a helper's return value, because the code it reaches is free to
//! extend it out of this file's sight. A site that genuinely creates
//! nothing declares itself in `DECLARED_JOINS` and names the guard or the
//! reason it needs none.
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
    /// The line's own text, trimmed, which must appear exactly `times`.
    line: &'static str,
    /// How many times the file holds that line, checked exactly: a copy
    /// the exemption was not granted to cannot inherit it, and a copy that
    /// has gone leaves the exemption naming nothing.
    times: usize,
    /// Why this one creates nothing a guard would have to remove.
    grounds: &'static str,
}

const DECLARED_JOINS: &[DeclaredJoin] = &[
    DeclaredJoin {
        file: "view-ai/src/config.rs",
        line: r#"let missing = std::env::temp_dir().join("view-ai-config-does-not-exist.toml");"#,
        times: 1,
        grounds: "it names a path that must NOT exist, and a guard that created \
                  a directory there would be the thing that broke it",
    },
    DeclaredJoin {
        file: "view-ai/tests/session.rs",
        line: "std::fs::canonicalize(std::env::temp_dir())",
        times: 1,
        grounds: "it spells the path the stub agent writes from its own side, \
                  for an equality assertion; this side creates nothing there",
    },
    // The launcher sites: every one of these hands the root on as the
    // working directory a spawned agent is started in. A cwd is read, never
    // extended -- the callee creates no name under it -- so there is
    // nothing for a guard to remove, and the process that would have
    // written there is a stub or a binary that does not exist.
    DeclaredJoin {
        file: "view-ai/tests/fixtures/drop_harness.rs",
        line: "let cfg = AgentLaunch::new(agent, std::env::temp_dir()).with_args([",
        times: 1,
        grounds: "the root is the launch's cwd, which the spawn reads and \
                  never extends",
    },
    DeclaredJoin {
        file: "view-ai/tests/session.rs",
        line: "std::env::temp_dir(),",
        times: 2,
        grounds: "the root is the launch's cwd, which the spawn reads and \
                  never extends",
    },
    DeclaredJoin {
        file: "view-ai/tests/session.rs",
        line: r#"let cfg = AgentLaunch::new("view-ai-no-such-agent-on-any-path", std::env::temp_dir());"#,
        times: 1,
        grounds: "the root is the launch's cwd, and the agent named does not \
                  exist, so nothing is ever spawned there at all",
    },
    DeclaredJoin {
        file: "view-ai/src/acp/driver.rs",
        line: "std::env::temp_dir(),",
        times: 2,
        grounds: "the root is the driver's cwd, which the spawn reads and \
                  never extends",
    },
    DeclaredJoin {
        file: "view-ai/src/acp/driver.rs",
        line: "let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);",
        times: 6,
        grounds: "the root is the driver's cwd, which the spawn reads and \
                  never extends",
    },
    DeclaredJoin {
        file: "view-ai/src/acp/driver.rs",
        line: "Driver::new(shared, out_tx, std::env::temp_dir(), requires_auth),",
        times: 1,
        grounds: "the root is the driver's cwd, which the spawn reads and \
                  never extends",
    },
    DeclaredJoin {
        file: "view-ai/src/acp/driver.rs",
        line: "Driver::new(shared, out_tx, std::env::temp_dir(), false),",
        times: 1,
        grounds: "the root is the driver's cwd, which the spawn reads and \
                  never extends",
    },
    DeclaredJoin {
        file: "view-ai/src/acp/session.rs",
        line: "let cfg = AgentLaunch::from_adapter(&adapter, std::env::temp_dir());",
        times: 1,
        grounds: "the root is the launch's cwd, which the spawn reads and \
                  never extends",
    },
];

#[test]
fn no_test_assembles_a_scratch_path_outside_the_guard() {
    let mut undeclared = Vec::new();
    for (name, source) in common::workspace_test_sources() {
        if name.ends_with(SELF_SOURCE) || name == GUARD_SOURCE {
            continue;
        }
        for (number, line) in temp_root_escapes(&source) {
            if is_declared(&name, &line) {
                continue;
            }
            undeclared.push(format!("{name}:{number}: {line}"));
        }
    }
    assert!(
        undeclared.is_empty(),
        "these tests let the system temp root grow a name of their own -- \
         by joining onto it, by pushing onto it, or by binding it to a \
         variable the next statement can push onto -- so whatever they \
         create there outlives a failing assertion and outlives the \
         run:\n  {}\nTake the path from view_test_support::ScratchDir \
         instead -- it removes the tree on every exit path, panic \
         included -- or, if the site creates nothing at all, add it to \
         DECLARED_JOINS with the grounds that say so",
        undeclared.join("\n  ")
    );
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
        "joined on one line",
        r#"
let dir = std::env::temp_dir().join(format!("view-thing-{}", std::process::id()));
"#,
        2,
    ),
    (
        "joined on the line rustfmt moved it to",
        r#"
let dir = std::env::temp_dir()
    .join(format!("view-other-{}", std::process::id()));
"#,
        2,
    ),
    (
        "joined onto a directly imported temp_dir",
        r#"
fn imported_directly() -> PathBuf {
    temp_dir().join("view-third")
}
"#,
        3,
    ),
    (
        "bound, then pushed onto by the next statement",
        r#"
let mut dir = std::env::temp_dir();
dir.push(format!("view-fourth-{}", std::process::id()));
"#,
        2,
    ),
    (
        "bound in one statement and joined in the next",
        r#"
let base = std::env::temp_dir();
base.join("view-fifth")
"#,
        2,
    ),
    (
        "joined through a conversion",
        r#"
let dir = PathBuf::from(std::env::temp_dir()).join("view-sixth");
"#,
        2,
    ),
    (
        "taken as a path first, then joined",
        r#"
let dir = std::env::temp_dir().to_path_buf().join("view-seventh");
"#,
        2,
    ),
    (
        "handed on as a call's argument",
        r#"
let capture = Capture::open_in(&std::env::temp_dir());
"#,
        2,
    ),
    (
        "handed back by a helper",
        r#"
fn scratch_root() -> PathBuf {
    std::env::temp_dir()
}
"#,
        3,
    ),
    (
        "stored in a struct field",
        r#"
let fixture = Fixture {
    base: std::env::temp_dir(),
};
"#,
        3,
    ),
];

/// The spellings the rule does not ask about, which the walk must leave
/// alone: a rule that flagged these would be unstatable in its own
/// documentation and would refuse the guard it points every site at.
const HELD_SHAPES: &str = r#"
// a comment naming std::env::temp_dir().join("anything") is prose
let guarded = ScratchDir::new("label").unwrap().join("file.txt");
let elsewhere = fixture_root().join("view-eighth");
"#;

#[test]
fn the_walk_sees_the_root_escape_however_it_is_written() {
    for (shape, source, at) in ESCAPING_SHAPES {
        let found: Vec<usize> = temp_root_escapes(source)
            .into_iter()
            .map(|(number, _)| number)
            .collect();
        assert_eq!(
            found,
            vec![*at],
            "the walk read {found:?} of the fixture for a root {shape}, \
             which is at line {at}. A shape it misses is a leak the \
             population can carry unnoticed; a line it adds is a site the \
             rule does not ask about being reported as a violation"
        );
    }
}

#[test]
fn the_walk_leaves_the_spellings_the_rule_does_not_ask_about_alone() {
    let found = temp_root_escapes(HELD_SHAPES);
    assert!(
        found.is_empty(),
        "the walk reported {found:?}, so it flags a spelling the rule does \
         not ask about -- including the guard it tells every site to use"
    );
}

/// Every place in `source` that lets the system temp root grow a name, as
/// (line number, the line's text on one line).
///
/// Read a statement at a time rather than a line at a time, because
/// rustfmt puts the `.join(` on the line after the call as soon as the name
/// is long enough, and a line-at-a-time reader sees neither half. Reported
/// at the line the call itself is on, which is where a reader will find it.
fn temp_root_escapes(source: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.contains("temp_dir()") || is_comment(trimmed) {
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
        if extends_the_root(&statement)
            || binds_the_root(&statement)
            || hands_the_root_onward(&statement)
        {
            found.push((index + 1, trimmed.to_string()));
        }
    }
    found
}

/// Whether the line is prose rather than code: a rule quoting the shape it
/// forbids creates no directory.
fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//") || trimmed.starts_with('*')
}

/// Whether `statement` hangs a name off the temp root in one expression,
/// however the call is wrapped -- `.join(` and `.push(` reach it through
/// `PathBuf::from(..)` and `.to_path_buf()` alike.
fn extends_the_root(statement: &str) -> bool {
    statement
        .find("temp_dir()")
        .map(|at| &statement[at..])
        .is_some_and(|after| after.contains(".join(") || after.contains(".push("))
}

/// Whether `statement` binds the bare temp root to a name.
///
/// The name is the escape: the statement that pushes onto it is a different
/// statement, and reading one statement at a time can never see it. So the
/// binding is the violation, not what a later line does with it -- which is
/// also the shape that leaked 117 directories on the macOS host. A call
/// handed straight to something else binds nothing and is left alone.
fn binds_the_root(statement: &str) -> bool {
    let Some((binding, value)) = statement.split_once('=') else {
        return false;
    };
    binding.trim_start().starts_with("let ")
        && value
            .trim()
            .trim_end_matches(';')
            .trim_end()
            .ends_with("temp_dir()")
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

/// Whether `statement` hands the bare temp root onward -- as a call's
/// argument, as a struct field's value, or as the value a helper returns.
///
/// None of those extends the root here, and every one of them puts it
/// where code outside this file may: `Capture::open_in(&std::env::temp_dir())`
/// joins onto its `base`, and reading the call site alone can never say
/// so. The rule is therefore that the site declares itself, and the
/// declaration names the guard -- which for `open_in` is `Capture`'s own
/// `Drop`.
fn hands_the_root_onward(statement: &str) -> bool {
    let Some(at) = statement.find("temp_dir()") else {
        return false;
    };
    let before = statement[..at]
        .trim_end_matches("temp_dir")
        .trim_end_matches("::")
        .trim_end_matches("std::env")
        .trim_end_matches('&')
        .trim_end();
    before.is_empty()
        || before.ends_with('(')
        || before.ends_with(',')
        || before.ends_with(':')
        || before.ends_with("return")
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
            hits, declared.times,
            "{}: the exempted line occurs {hits} times, not {} -- an \
             exemption that names nothing exempts nothing, and one that \
             names more lines than it was written for was granted to only \
             some of them:\n  {}",
            declared.file, declared.times, declared.line
        );
    }
}
