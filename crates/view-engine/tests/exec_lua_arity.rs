//! The walk that keeps every `nvim_exec_lua` call in this crate at the two
//! parameters nvim requires: the chunk, and the array of its arguments.
//!
//! nvim refuses any other count with an error reply, and an async waiter
//! resolves that reply to its safe default, so a call with one parameter
//! answers every time and answers nothing. The picker's buffer list shipped
//! that way and opened empty in every session.
//!
//! Every source file under `crates/view-engine/src` is read down to the
//! `#[cfg(test)]` module that closes it, so a call added anywhere in the
//! shipped code is checked with no edit here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

const METHOD: &str = "\"nvim_exec_lua\"";

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a readable source directory") {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            rust_sources(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

/// `source` down to the line before its `#[cfg(test)]` module, with every
/// comment line blanked so line numbers still match the file.
fn shipped(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = String::new();
    for (at, line) in lines.iter().enumerate() {
        let next = lines.get(at + 1).map_or("", |l| l.trim_start());
        if line.trim() == "#[cfg(test)]" && next.starts_with("mod ") {
            break;
        }
        if !line.trim_start().starts_with("//") {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The number of top-level elements in the bracketed list `text` opens
/// with, skipping what string literals hold, or `None` when it never
/// closes.
fn top_level_elements(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut count = 0usize;
    let mut segment_has_content = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                while let Some(s) = chars.next() {
                    match s {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
                segment_has_content = true;
            }
            '[' | '(' | '{' => {
                depth += 1;
                if depth > 1 {
                    segment_has_content = true;
                }
            }
            ']' | ')' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(count + usize::from(segment_has_content));
                }
            }
            ',' if depth == 1 => {
                count += 1;
                segment_has_content = false;
            }
            c if !c.is_whitespace() => segment_has_content = true,
            _ => {}
        }
    }
    None
}

/// Every `"nvim_exec_lua"` site in `source`, by line number, with the
/// number of parameters its `vec![...]` carries, or `None` when no
/// parameter list follows the method name in the same call.
fn sites(source: &str) -> Vec<(usize, Option<usize>)> {
    let text = shipped(source);
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find(METHOD) {
        let at = from + offset;
        let line = text[..at].matches('\n').count() + 1;
        let after = &text[at + METHOD.len()..];
        let params = after.find("vec![").and_then(|gap| {
            let between = &after[..gap];
            // `, ` between arguments, or `.to_owned(), params: ` inside an
            // RpcMessage literal, and never a statement boundary
            let spelled = between.chars().filter(|c| !c.is_whitespace()).count();
            let same_call = !between.contains([';', '{', '}']) && spelled <= 24;
            same_call
                .then(|| top_level_elements(&after[gap + "vec!".len()..]))
                .flatten()
        });
        found.push((line, params));
        from = at + METHOD.len();
    }
    found
}

#[test]
fn every_exec_lua_call_passes_the_chunk_and_its_argument_array() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    sources.sort();
    let mut checked = 0;
    let mut wrong = Vec::new();
    for path in &sources {
        let source = std::fs::read_to_string(path).expect("a readable source file");
        for (line, params) in sites(&source) {
            checked += 1;
            if params != Some(2) {
                wrong.push(format!(
                    "{}:{line}: {}",
                    path.display(),
                    params.map_or_else(
                        || "no vec![...] parameter list follows".to_owned(),
                        |n| format!("{n} parameters"),
                    )
                ));
            }
        }
    }
    assert!(
        checked > 0,
        "no nvim_exec_lua call found under {}",
        root.display()
    );
    assert!(
        wrong.is_empty(),
        "nvim_exec_lua takes exactly (chunk, args), and these calls send \
         another count:\n{}",
        wrong.join("\n")
    );
}

/// The counter reads nesting, strings and a trailing comma the way the
/// compiler does.
#[test]
fn the_parameter_count_reads_only_the_top_level() {
    assert_eq!(top_level_elements("[]"), Some(0));
    assert_eq!(top_level_elements("[Value::from(CHUNK)]"), Some(1));
    assert_eq!(
        top_level_elements("[\n  Value::from(\"a, ]\"),\n  Value::Array(vec![x, y]),\n]"),
        Some(2)
    );
    assert_eq!(top_level_elements("[f(a, b), [c, d], { e }]"), Some(3));
    assert_eq!(top_level_elements("[a, b"), None);
}

/// A test module's own calls are outside the walk, and a call written
/// above it is inside.
#[test]
fn the_walk_stops_at_the_test_module() {
    let source = [
        "fn shipped() {",
        "    // \"nvim_exec_lua\", vec![one]",
        "    h.request(\"nvim_exec_lua\", vec![one]);",
        "}",
        "#[cfg(test)]",
        "mod tests {",
        "    fn fixture() { h.request(\"nvim_exec_lua\", vec![]); }",
        "}",
    ]
    .join("\n");
    assert_eq!(sites(&source), vec![(3, Some(1))]);
}
