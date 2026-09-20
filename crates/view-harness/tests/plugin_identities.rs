//! The walk that keeps a plugin's name out of the code that decides what
//! view draws.
//!
//! Ownership is decided by the channel a holder writes, so the production
//! code has no reason to know which plugin wrote it: a plugin released
//! tomorrow reaches the screen through the same options, capabilities,
//! replaced globals and floating windows an enumerated one does, and view
//! answers all four the same way. A name in that code is a second answer
//! for one of them, and it is the answer that goes stale.
//!
//! [`IDENTITIES`] is the list of names this tree once carried, not a
//! specification of what is refused: any plugin identity is refused, and a
//! new one is caught by review or by the name arriving here. The Lua module
//! registry is on the list for the same reason -- reading it is how a
//! session asks which plugins are loaded, which is a question view no
//! longer has.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

/// The directories whose code decides what view draws, what it takes over
/// and what it tells a user about.
const ROOTS: [&str; 5] = [
    "crates/view-core/src/native",
    "crates/view-native/src",
    "crates/view-engine/src",
    "crates/view/src",
    "crates/view-surface/src",
];

/// The plugin identities this tree shipped, in every spelling it shipped
/// them: a plugin's own name, the Lua module a session would `require`,
/// and the filetype its windows present.
const IDENTITIES: [&str; 12] = [
    "noice",
    "notify.nvim",
    "nvim-notify",
    "lualine",
    "bufferline",
    "barbecue",
    "dropbar",
    "telescope",
    "cmp",
    "fidget",
    "dressing",
    "package.loaded",
];

/// `cmp` is also the standard library's ordering module and the method
/// every `Ord` carries, so an occurrence reached through a path or a
/// receiver is Rust's and not a plugin's. Nothing else on the list is a
/// word in Rust.
fn is_rust_cmp(line: &str, at: usize) -> bool {
    line[..at].trim_end().ends_with([':', '.'])
}

/// Every occurrence of `needle` in `line` that is not part of a longer
/// word to its left, lowercased by the caller.
///
/// Open to the right on purpose: a filetype glues the plugin's name to its
/// widget (`cmp_menu`), which a word boundary on that side would let
/// through.
fn hits(line: &str, needle: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = line[from..].find(needle) {
        let at = from + offset;
        let left_is_word = line[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !left_is_word {
            found.push(at);
        }
        from = at + needle.len();
    }
    found
}

/// `path`'s lines with every `#[cfg(test)]` item taken out, numbered from
/// one.
///
/// A test names the plugins it runs against, which is the coverage the
/// ruling kept; only the shipped code is walked. The item runs from the
/// attribute to the `}` in the first column that closes it, which is the
/// shape `task fmt` guarantees for a top-level item.
fn shipped_lines(source: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut in_test = false;
    for (index, line) in source.lines().enumerate() {
        if in_test {
            if line == "}" {
                in_test = false;
            }
            continue;
        }
        if line == "#[cfg(test)]" {
            in_test = true;
            continue;
        }
        lines.push((index + 1, line));
    }
    lines
}

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("a readable source directory");
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            rust_sources(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels under the repo root")
        .to_path_buf()
}

#[test]
fn no_shipped_ownership_code_names_a_plugin() {
    let root = repo_root();
    let mut sources = Vec::new();
    for dir in ROOTS {
        rust_sources(&root.join(dir), &mut sources);
    }
    assert!(
        sources.len() > 50,
        "the walk found {} files, which is not the tree it is written for",
        sources.len()
    );
    let mut named = Vec::new();
    for path in &sources {
        let source = std::fs::read_to_string(path).expect("a readable source file");
        for (number, line) in shipped_lines(&source) {
            let lowered = line.to_ascii_lowercase();
            for identity in IDENTITIES {
                for at in hits(&lowered, identity) {
                    if identity == "cmp" && is_rust_cmp(&lowered, at) {
                        continue;
                    }
                    let shown = path.strip_prefix(&root).unwrap_or(path).display();
                    named.push(format!("{shown}:{number}: {identity}: {}", line.trim()));
                }
            }
        }
    }
    assert!(
        named.is_empty(),
        "shipped code names a plugin; ownership is decided by the channel a holder \
         writes, so the name has nothing to decide:\n{}",
        named.join("\n")
    );
}

/// The walk's own reading, so a rewrite of it cannot quietly stop finding
/// anything.
#[test]
fn the_walk_reads_a_name_where_one_stands_and_nowhere_else() {
    assert_eq!(hits("cmp_menu", "cmp"), vec![0]);
    assert_eq!(hits("nvim-cmp", "cmp"), vec![5]);
    assert!(hits("recmp", "cmp").is_empty());
    assert!(is_rust_cmp("std::cmp::reverse", 5));
    assert!(is_rust_cmp("a.cmp(&b)", 2));
    assert!(!is_rust_cmp("nvim-cmp", 5));
    let source = "live\n#[cfg(test)]\nmod tests {\n    fn noice() {}\n}\nalso\n";
    let shipped: Vec<&str> = shipped_lines(source).into_iter().map(|(_, l)| l).collect();
    assert_eq!(shipped, vec!["live", "also"]);
}
