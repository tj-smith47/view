//! Source-text pins on the two shapes `set -euo pipefail` turns into a
//! wrong answer the script cannot report.
//!
//! Every acceptance script runs under `set -euo pipefail`, and the shape
//! they all read source constants with is
//! `NAME=$(grep ... | sed ...)` followed by `[ -n "$NAME" ] || { ... }`.
//! When the grep matches nothing, `pipefail` gives the assignment status 1
//! and `errexit` ends the script *before* the guard below it runs: the leg
//! dies on rc=1 with none of the diagnosis the guard was written to print,
//! which is the failure mode a leg's guard exists to prevent. A `|| true`
//! on the assignment costs nothing and hands the empty value to the guard,
//! which then says what moved.
//!
//! The second shape is the same interpreter setting read from the other
//! end: `producer | grep -q needle`. A quiet grep exits at its first match
//! and the producer dies of SIGPIPE, so `pipefail` gives the pipeline a
//! non-zero status for the run that *found* what it was looking for -- a
//! match read as a miss, in a condition whose whole job is to tell those
//! apart. The reader either takes a captured string (`case`, `[[ =~ ]]`)
//! or drops `-q` and tests what it captured, and then no pipe can fail.
//!
//! The third is not an interpreter setting but the same kind of silence: a
//! script that sources the shared helper and then defines a function the
//! helper already defines gets whichever body loaded last, and nothing
//! says which rule it is running under. The two spellings of `tmux_key`
//! that lived in two legs are the case in hand -- one refused bracket
//! notation, the other stripped the brackets, and the file you opened
//! decided which.
//!
//! Housed beside `macos_clock.rs` and `scratch_dirs.rs` because it is the
//! same kind of rule: one the next script has to trip over mechanically
//! rather than be told about.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// The commands whose "found nothing" is a status, not an empty string --
/// the ones that turn a missing match into an exit before the guard.
const FALLIBLE: [&str; 2] = ["grep", "pgrep"];

/// How far past an assignment a guard on the same name still counts as
/// that assignment's guard: far enough for the comment and the second
/// assignment the real sites put in between, short enough that an
/// unrelated later test of the same name is not read as one.
const GUARD_REACH: usize = 600;

/// The helper every acceptance script sources, and the one place a reader
/// they all use belongs.
const SHARED_HELPER: &str = "scripts/acceptance/artifacts.sh";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("this crate sits two levels under the workspace root")
        .to_owned()
}

/// Every checked-in shell script the two rules govern, as (path relative to
/// the root, text), with the population itself asserted: a walk that has
/// stopped finding scripts passes both rules by reading nothing.
fn governed_scripts() -> Vec<(String, String)> {
    let root = repo_root();
    // `.claude/hooks` as well as `scripts`: the hooks are checked in, run
    // under the same `set -euo pipefail`, and are the population the next
    // one of these gets written into
    let mut scripts: Vec<PathBuf> = ["scripts", ".claude/hooks"]
        .iter()
        .flat_map(|dir| shell_scripts(&root.join(dir)))
        .collect();
    scripts.sort();
    assert!(
        scripts.len() > 5,
        "the walk found only {} scripts, so it is not looking where they \
         live and would pass by finding nothing",
        scripts.len()
    );
    for dir in ["scripts", ".claude/hooks"] {
        assert!(
            scripts
                .iter()
                .any(|script| script.starts_with(root.join(dir))),
            "the walk found nothing under {dir}, so that half of the \
             population is not being read at all"
        );
    }
    scripts
        .into_iter()
        .map(|script| {
            let name = script
                .strip_prefix(&root)
                .unwrap_or(&script)
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&script).expect("a script must be readable");
            (name, text)
        })
        .collect()
}

/// Every function `text` defines at the top level, as (name, the line it
/// opens on).
///
/// Top level only: an indented definition is inside another function or a
/// heredoc, where it shadows nothing a source-time definition set.
fn functions_defined(text: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let Some((name, rest)) = line.split_once("()") else {
            continue;
        };
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            || !rest.trim_start().starts_with('{')
        {
            continue;
        }
        found.push((name.to_owned(), index + 1));
    }
    found
}

#[test]
fn no_script_redefines_what_the_shared_helper_defines() {
    let root = repo_root();
    let shared = std::fs::read_to_string(root.join(SHARED_HELPER))
        .expect("the shared helper must be readable");
    let shared_names: Vec<String> = functions_defined(&shared)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        shared_names.len() > 5,
        "the reader found {} functions in {SHARED_HELPER}, so it is not \
         reading it and every script below would pass by comparison with \
         nothing",
        shared_names.len()
    );
    let mut shadowed = Vec::new();
    for (name, text) in governed_scripts() {
        // the rule is about shadowing, so it asks only of the scripts that
        // load the helper: a script that never sources it defines its own
        // world and collides with nothing
        if name == SHARED_HELPER || !text.contains(SHARED_HELPER) {
            continue;
        }
        for (function, number) in functions_defined(&text) {
            if shared_names.contains(&function) {
                shadowed.push(format!("{name}:{number}: {function}"));
            }
        }
    }
    assert!(
        shadowed.is_empty(),
        "these define a function {SHARED_HELPER} already defines, and they \
         source it -- so the body that runs is whichever loaded last, and \
         two legs can be reading the same call under different rules with \
         nothing to say so:\n  {}\nMove the shared one into the helper and \
         delete the copies, or give the different job its own name",
        shadowed.join("\n  ")
    );
}

#[test]
fn the_definition_reader_sees_a_top_level_function_and_nothing_else() {
    let fixture = "
set -euo pipefail
. \"$SCRIPT_DIR/artifacts.sh\"
holds() { case \"$2\" in *\"$1\"*) return 0 ;; *) return 1 ;; esac; }
matches() {
    local line
}
outer() {
    inner() { printf 'nested'; }
}
pane \"$SESSION\"
";
    let found: Vec<(String, usize)> = functions_defined(fixture);
    assert_eq!(
        found,
        vec![
            ("holds".to_owned(), 4),
            ("matches".to_owned(), 5),
            ("outer".to_owned(), 8),
        ],
        "the reader must see a one-liner and a block at the top level, and \
         must leave a nested definition and a call alone -- a reader that \
         misses a spelling lets the shadowing it exists to catch through"
    );
}

#[test]
fn every_guarded_capture_can_reach_its_guard() {
    let mut unreachable = Vec::new();
    for (name, text) in governed_scripts() {
        for (number, line) in guards_behind_an_errexit(&text) {
            unreachable.push(format!("{name}:{number}: {line}"));
        }
    }
    assert!(
        unreachable.is_empty(),
        "these captures run a command that exits non-zero when it matches \
         nothing, and a guard below them tests the value it did not \
         produce -- under `set -euo pipefail` the script is already gone \
         when that guard would have run, so the leg fails on rc=1 with \
         none of its own diagnosis:\n  {}\nPut `|| true` on the \
         assignment: the guard is the diagnosis, and this is what lets it \
         run",
        unreachable.join("\n  ")
    );
}

#[test]
fn no_condition_reads_a_pipe_with_a_quiet_grep() {
    let mut quiet = Vec::new();
    for (name, text) in governed_scripts() {
        for (number, segment) in quiet_readers_behind_a_pipe(&text) {
            quiet.push(format!("{name}:{number}: {segment}"));
        }
    }
    assert!(
        quiet.is_empty(),
        "these readers sit on the receiving end of a pipe and exit at their \
         first match, killing the producer with SIGPIPE -- under `set -o \
         pipefail` the pipeline that found the text then reports the status \
         of the one that did not, so the condition reads a match as a \
         miss:\n  {}\nCapture the producer's output and test the string \
         (`holds`, `matches`, `case`, `[[ =~ ]]`), or drop `-q` and test \
         what the capture holds -- neither has a pipe to fail",
        quiet.join("\n  ")
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
        "a grep capture guarded by -n on the next line",
        "
set -euo pipefail
NAME=$(grep -oE 'const NAME: &str = \"[^\"]+\"' \"$FILE\" | sed -E 's/.*\"(.*)\"/\\1/')
[ -n \"$NAME\" ] || { printf 'FAIL: no NAME\\n' >&2; exit 1; }
",
        3,
    ),
    (
        "a capture rustfmt-style wrapped over two lines, guarded by -z",
        "
set -euo pipefail
wrapped=$(grep -oE \"^($ONE|$TWO)$\" \"$FILE\" |
    head -1 | cut -d: -f1)
if [ -z \"$wrapped\" ]; then
    exit 1
fi
",
        3,
    ),
    (
        "a pgrep capture guarded by -n",
        "
set -euo pipefail
pid=$(pgrep -P \"$parent\" -x view | head -1)
[ -n \"$pid\" ] || return 1
",
        3,
    ),
];

/// The spellings the rule does not ask about, which the walk must leave
/// alone: a capture that already forces its own status either side of the
/// closing paren, a command whose "found nothing" is an empty string
/// rather than a status, and a capture with no guard below it at all.
const HELD_SHAPES: &str = "
set -euo pipefail
held=$(grep -oE 'held' \"$FILE\" | head -1) || true
[ -n \"$held\" ] || return 1
counted=$(grep -c . \"$FILE\" || true)
[ -n \"$counted\" ] || return 1
unguarded=$(grep -oE 'nothing tests this' \"$FILE\")
shaped=$(printf '%s\\n' \"$rows\" | awk '{ print $1 }')
[ -n \"$shaped\" ] || return 1
";

/// The quiet-reader shapes the second walk must see, on the same terms as
/// [`ESCAPING_SHAPES`]: one fixture per shape, each carrying the line the
/// walk must report.
const PIPED_QUIET_SHAPES: &[(&str, &str, usize)] = &[
    (
        "a capture piped into a quiet grep on one line",
        "
set -euo pipefail
if ! pane | grep -qF -- \"$text\"; then
    exit 1
fi
",
        3,
    ),
    (
        "a pipeline wrapped over two lines",
        "
set -euo pipefail
grep -A 2 -- '^\\[chrome\\]' \"$FILE\" |
    grep -qE \"^bg = $DECIMAL$\"
",
        3,
    ),
    (
        "a quiet grep three segments down",
        "
set -euo pipefail
if grep 'a row' \"$LOG\" | tail -n +2 | grep -qvF \"$id\"; then
    exit 1
fi
",
        3,
    ),
    (
        "a quiet grep behind an environment assignment",
        "
set -euo pipefail
if ! pane | LC_ALL=C grep -qF -- \"$text\"; then
    exit 1
fi
",
        3,
    ),
    (
        "a quiet grep behind a wrapper that runs it",
        "
set -euo pipefail
if ! journal | sudo grep -qF -- \"$text\"; then
    exit 1
fi
",
        3,
    ),
];

/// The spellings the second rule does not ask about, which its walk must
/// leave alone: a quiet grep reading a file, a pipeline whose reader drains
/// its input, an alternation inside the pattern rather than a pipeline, the
/// `case` reader that replaced the shape, a reader behind a pipe and a
/// wrapper that does not exit at its first match, and prose.
const HELD_QUIET_SHAPES: &str = "
set -euo pipefail
grep -qF -- 'held' \"$FILE\" || exit 1
holds() { case \"$2\" in *\"$1\"*) return 0 ;; *) return 1 ;; esac; }
matched=$(pane | grep -F -- \"$text\") || true
boxed=$(box_text | LC_ALL=C grep -F -- \"$text\") || true
grep -qE \"^($ONE|$TWO)$\" \"$FILE\" || exit 1
# the old shape, piped into | grep -q, named in prose
shaped=$(printf '%s\\n' \"$rows\" | awk '{ print $1 }')
";

#[test]
fn the_second_walk_sees_a_quiet_reader_however_the_pipeline_is_written() {
    // every row is read before the assertion fires: a walk that has
    // stopped seeing two spellings names both, where an assertion inside
    // the loop would hide the second behind the first
    let mut misread = Vec::new();
    for (shape, source, at) in PIPED_QUIET_SHAPES {
        let found: Vec<usize> = quiet_readers_behind_a_pipe(source)
            .into_iter()
            .map(|(number, _)| number)
            .collect();
        if found != vec![*at] {
            misread.push(format!(
                "the walk read {found:?} of the fixture for {shape}, which \
                 is at line {at}"
            ));
        }
    }
    assert!(
        misread.is_empty(),
        "a shape it misses is a condition that can read a match as a miss \
         unnoticed; a line it adds is a reader the rule does not ask about \
         being reported as a violation:\n  {}",
        misread.join("\n  ")
    );
}

#[test]
fn the_second_walk_leaves_the_readers_the_rule_does_not_ask_about_alone() {
    let found = quiet_readers_behind_a_pipe(HELD_QUIET_SHAPES);
    assert!(
        found.is_empty(),
        "the walk reported {found:?}, so it flags a reader the rule does not \
         ask about -- one with no pipe feeding it, one that drains what \
         feeds it, an alternation inside a quoted pattern read as a \
         pipeline, or a mention in a comment"
    );
}

#[test]
fn the_walk_sees_an_unreachable_guard_however_it_is_written() {
    // every row is read before the assertion fires: a walk that has
    // stopped seeing two spellings names both, where an assertion inside
    // the loop would hide the second behind the first
    let mut misread = Vec::new();
    for (shape, source, at) in ESCAPING_SHAPES {
        let found: Vec<usize> = guards_behind_an_errexit(source)
            .into_iter()
            .map(|(number, _)| number)
            .collect();
        if found != vec![*at] {
            misread.push(format!(
                "the walk read {found:?} of the fixture for {shape}, which is \
                 at line {at}"
            ));
        }
    }
    assert!(
        misread.is_empty(),
        "a shape it misses is a leg that can die with no diagnosis \
         unnoticed; a line it adds is a capture the rule does not ask \
         about being reported as a violation:\n  {}",
        misread.join("\n  ")
    );
}

#[test]
fn the_walk_leaves_the_captures_the_rule_does_not_ask_about_alone() {
    let found = guards_behind_an_errexit(HELD_SHAPES);
    assert!(
        found.is_empty(),
        "the walk reported {found:?}, so it flags a capture the rule does \
         not ask about -- one that already forces its own status, one whose \
         command cannot fail on an empty match, or one with no guard at all"
    );
}

/// Every `.sh` file under `dir`, recursively.
fn shell_scripts(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(shell_scripts(&path));
        } else if path.extension().is_some_and(|ext| ext == "sh") {
            found.push(path);
        }
    }
    found
}

/// Every capture in `text` whose command can exit non-zero and whose guard
/// below it therefore never runs, as (line number, the assignment's first
/// line trimmed).
fn guards_behind_an_errexit(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    for (index, line) in text.lines().enumerate() {
        let start = at;
        at += line.len() + 1;
        let Some(name) = captured_name(line) else {
            continue;
        };
        let open = start + line.find("=$(").unwrap_or_default() + 1;
        let Some(end) = substitution_end(bytes, open) else {
            continue;
        };
        if !FALLIBLE.iter().any(|cmd| text[open..end].contains(cmd)) {
            continue;
        }
        // either side of the closing paren: `$(... || true)` forces the
        // substitution's own status the same way `$(...) || true` forces
        // the assignment's
        let rest_of_line = text[end..].lines().next().unwrap_or_default();
        if rest_of_line.contains("||") || text[open..end].contains("|| true") {
            continue;
        }
        let after = &text[end..text.len().min(end + GUARD_REACH)];
        if [format!("-n \"${name}\""), format!("-z \"${name}\"")]
            .iter()
            .any(|test| after.contains(test.as_str()))
        {
            found.push((index + 1, line.trim().to_string()));
        }
    }
    found
}

/// Every reader in `text` that exits at its first match while a pipe feeds
/// it, as (the line its pipeline opens on, the offending segment trimmed).
///
/// A pipeline is read as the shell reads it: comments dropped, a trailing
/// `|` or `\` continuing onto the next line, and a `|` inside quotes
/// splitting nothing -- an alternation in a pattern is not a pipeline.
fn quiet_readers_behind_a_pipe(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut statement = String::new();
    let mut opened_at = 0;
    for (index, line) in text.lines().enumerate() {
        let code = code_before_comment(line);
        if statement.is_empty() {
            if code.trim().is_empty() {
                continue;
            }
            opened_at = index + 1;
        }
        statement.push_str(code);
        if continues_onto_the_next_line(code) {
            statement.push(' ');
            continue;
        }
        found.extend(
            pipeline_segments(&statement)
                .into_iter()
                .skip(1)
                .filter(|segment| reads_quietly(segment))
                .map(|segment| (opened_at, segment.trim().to_string())),
        );
        statement.clear();
    }
    found
}

/// `line` up to the `#` that starts a comment on it, if one does.
///
/// Quote state is per line rather than carried across them, which is the
/// walk's one stated ceiling: a quoted `#` inside a string a statement
/// continues past would end the reading of that line early. Carrying the
/// state instead would desync the whole file on the first heredoc, so the
/// narrower reading is the safer one -- the truncation can only lose code
/// from the line, never add any.
///
/// What the statement walk built on it then reports can go either way: a
/// heredoc body or a quoted block whose line reads as a pipeline is read
/// as one, and can be reported where no condition exists. That direction
/// is loud -- a named line in a failing assertion, not a silent miss -- so
/// it is the safe one to be wrong in; a second, differently-written sweep
/// of the population is what says nothing is being missed today.
fn code_before_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    for (index, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            // a `#` is a comment only where a word can start, which is why
            // `$x#y` and `a#b` are not comments
            None if byte == b'#'
                && index
                    .checked_sub(1)
                    .is_none_or(|before| bytes[before].is_ascii_whitespace()) =>
            {
                return &line[..index];
            }
            None => {}
        }
    }
    line
}

/// Whether the shell would read the next line as part of this statement: a
/// line ending in a backslash, or in the `|` a pipeline is continued past.
fn continues_onto_the_next_line(code: &str) -> bool {
    let trimmed = code.trim_end();
    trimmed.ends_with('\\') || (trimmed.ends_with('|') && !trimmed.ends_with("||"))
}

/// `statement` split at the `|` characters the shell would, so segment `n`
/// reads what segment `n - 1` writes.
fn pipeline_segments(statement: &str) -> Vec<&str> {
    let bytes = statement.as_bytes();
    let mut segments = Vec::new();
    let mut quote: Option<u8> = None;
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(b'"') if byte == b'\\' => index += 1,
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            // `||` runs a command on a status, it does not feed one
            None if byte == b'|' => {
                if bytes.get(index + 1) == Some(&b'|') {
                    index += 1;
                } else if index == 0 || bytes[index - 1] != b'|' {
                    segments.push(&statement[start..index]);
                    start = index + 1;
                }
            }
            None => {}
        }
        index += 1;
    }
    segments.push(&statement[start..]);
    segments
}

/// Whether `segment` runs a grep that exits at its first match, which is
/// what leaves the pipeline's status describing the producer's death rather
/// than the search's answer.
///
/// The reader is looked for past what can stand in front of it and change
/// nothing about it: a negation, a `NAME=value` the command runs with, and
/// the wrappers that run another command. A wrapper's own flags are where
/// this stops -- `sudo -n grep -q` reads as no grep at all -- which is the
/// second stated ceiling of the walk, beside the per-line quote state at
/// [`code_before_comment`]. This one can only under-report; the population
/// carries neither spelling today.
fn reads_quietly(segment: &str) -> bool {
    let mut words = segment.split_whitespace().skip_while(|word| {
        matches!(*word, "!" | "sudo" | "env" | "command" | "nice" | "stdbuf")
            || word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            })
    });
    if !words
        .next()
        .is_some_and(|word| matches!(word, "grep" | "egrep" | "fgrep" | "zgrep"))
    {
        return false;
    }
    words.any(|word| word.starts_with('-') && word != "--" && word.contains('q'))
}

/// The name `line` captures a command substitution into, if it does.
fn captured_name(line: &str) -> Option<&str> {
    let head = line.split("=$(").next()?;
    if head.len() == line.len() {
        return None;
    }
    let name = head
        .trim()
        .rsplit(' ')
        .next()
        .filter(|name| !name.is_empty())?;
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        .then_some(name)
}

/// The byte just past the `)` closing the `$(` at `open`, tracking the
/// quotes a shell tracks -- a `)` inside `"^($A|$B)$"` closes nothing.
fn substitution_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 1_usize;
    let mut quote: Option<u8> = None;
    let mut index = open + 2;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(b'"') if byte == b'\\' => index += 1,
            Some(open_quote) if byte == open_quote => quote = None,
            Some(_) => {}
            None => match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index + 1);
                    }
                }
                _ => {}
            },
        }
        index += 1;
    }
    None
}
