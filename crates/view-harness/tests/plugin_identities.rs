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
//!
//! A comment is read like any other line. A name that decides nothing today
//! is the name the next reader keys a gate on, and the removal that made
//! this walk true was a sweep of the names on a list rather than of the
//! class they belong to.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

/// The crate source trees this walk does not read, each with why a plugin
/// name there is the plugin itself rather than a rule about it.
///
/// Everything else under `crates/*/src` is read, so a crate or a module
/// added tomorrow is covered with no edit here. A fixed list of roots got
/// exactly that wrong: the code deciding ownership by a holder's name
/// lived in a directory no root named.
const NOT_OWNERSHIP_CODE: [(&str, &str); 4] = [
    (
        "crates/view-bench/src",
        "the bench drivers build the configs they measure, and a measured config is a \
         named plugin stack",
    ),
    (
        "crates/view-harness/src",
        "the harness writes the fixture configs and keys the compat suite on scenario \
         names, which are the plugins under test",
    ),
    (
        "crates/view-oracle/src",
        "the oracle sorts plugins into compat classes, which is the question it exists \
         to answer",
    ),
    (
        "crates/view-test-support/src",
        "a crate only tests link, whose fixtures name what they load",
    ),
];

/// The plugin identities this tree shipped, in every spelling it shipped
/// them: a plugin's own name, the Lua module a session would `require`,
/// and the filetype its windows present.
const IDENTITIES: [&str; 17] = [
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
    "which-key",
    "gitsigns",
    "lazy.nvim",
    "nvim-treesitter",
    "neo-tree",
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

/// Every `.rs` file under `crates/*/src` this walk reads: the whole tree
/// less [`NOT_OWNERSHIP_CODE`], less the test modules a file of their own
/// holds.
fn walked_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let crates = std::fs::read_dir(root.join("crates")).expect("a readable crates directory");
    for entry in crates {
        let src = entry.expect("a readable crates entry").path().join("src");
        if !src.is_dir() {
            continue;
        }
        let relative = src.strip_prefix(root).unwrap_or(&src).to_path_buf();
        let spelled = relative.to_string_lossy().replace('\\', "/");
        if NOT_OWNERSHIP_CODE.iter().any(|(dir, _)| *dir == spelled) {
            continue;
        }
        rust_sources(&src, &mut sources);
    }
    let out_of_line = out_of_line_test_modules(&sources);
    sources.retain(|path| !out_of_line.contains(path));
    sources
}

/// The files holding a `#[cfg(test)] mod name;` module, which the attribute
/// skip above cannot see: the attribute sits on the declaration in the
/// parent file and the module's own file carries nothing.
fn out_of_line_test_modules(sources: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for path in sources {
        let source = std::fs::read_to_string(path).expect("a readable source file");
        let mut lines = source.lines().peekable();
        while let Some(line) = lines.next() {
            if line.trim() != "#[cfg(test)]" {
                continue;
            }
            let Some(declaration) = lines.peek() else {
                continue;
            };
            let trimmed = declaration.trim();
            let Some(name) = trimmed
                .strip_prefix("mod ")
                .or_else(|| trimmed.strip_prefix("pub mod "))
                .and_then(|rest| rest.strip_suffix(';'))
            else {
                continue;
            };
            let Some(dir) = path.parent() else {
                continue;
            };
            found.push(dir.join(format!("{name}.rs")));
            let nested = dir.join(name);
            found.extend(sources.iter().filter(|p| p.starts_with(&nested)).cloned());
        }
    }
    found
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
    let sources = walked_sources(&root);
    assert!(
        sources.len() > 100,
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

    let root = repo_root();
    let sources = walked_sources(&root);
    let holds = |path: &str| sources.iter().any(|p| p.ends_with(path));
    assert!(
        holds("view-core/src/update/surfaces.rs"),
        "the module that decides what a surface is claimed by has to be read"
    );
    assert!(
        !holds("view-core/src/update/tests.rs"),
        "a test module in a file of its own names the plugins it runs against"
    );
    assert!(
        !holds("view-harness/src/fixture.rs"),
        "the fixture writer is the half of the tree that has to name plugins"
    );
}
