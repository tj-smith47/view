//! A source-text pin on the guards the acceptance scripts read a constant
//! out of the tree with.
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

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("this crate sits two levels under the workspace root")
        .to_owned()
}

#[test]
fn every_guarded_capture_can_reach_its_guard() {
    let root = repo_root();
    let mut scripts = shell_scripts(&root.join("scripts"));
    scripts.sort();
    assert!(
        scripts.len() > 5,
        "the walk found only {} scripts, so it is not looking where they \
         live and would pass by finding nothing",
        scripts.len()
    );

    let mut unreachable = Vec::new();
    for script in scripts {
        let name = script
            .strip_prefix(&root)
            .unwrap_or(&script)
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&script).expect("a script must be readable");
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

/// The shapes the walk must see, each wrong on purpose: a rule proved only
/// by a population that already satisfies it cannot tell "nothing is wrong"
/// from "nothing is being read".
const ESCAPING_SHAPES: &str = r#"
set -euo pipefail
NAME=$(grep -oE 'const NAME: &str = "[^"]+"' "$FILE" | sed -E 's/.*"(.*)"/\1/')
[ -n "$NAME" ] || { printf 'FAIL: no NAME\n' >&2; exit 1; }
wrapped=$(grep -oE "^($ONE|$TWO)$" "$FILE" |
    head -1 | cut -d: -f1)
if [ -z "$wrapped" ]; then
    exit 1
fi
pid=$(pgrep -P "$parent" -x view | head -1)
[ -n "$pid" ] || return 1
these_are_the_shapes_the_rule_asks_for() {
    held=$(grep -oE 'held' "$FILE" | head -1) || true
    [ -n "$held" ] || return 1
    counted=$(grep -c . "$FILE" || true)
    [ -n "$counted" ] || return 1
    unguarded=$(grep -oE 'nothing tests this' "$FILE")
    shaped=$(printf '%s\n' "$rows" | awk '{ print $1 }')
    [ -n "$shaped" ] || return 1
}
"#;

#[test]
fn the_walk_sees_an_unreachable_guard_however_it_is_written() {
    let at: Vec<usize> = guards_behind_an_errexit(ESCAPING_SHAPES)
        .into_iter()
        .map(|(number, _)| number)
        .collect();
    assert_eq!(
        at,
        vec![3, 5, 10],
        "the walk read {at:?} of the fixture. Every line it missed is a leg \
         that can die with no diagnosis unnoticed; every extra line is a \
         capture the rule does not ask about being reported as a violation"
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
