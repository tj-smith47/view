//! A source-text pin on how a bench driver waits.
//!
//! A driver blocks on the thing it waits for: the pty's output, a condvar,
//! a sleep. It never loops on `yield_now`, because on macOS a yield while
//! other work is runnable costs the caller a whole scheduler quantum, and a
//! driver waiting on a screen adds that quantum to the sample it times.
//!
//! The walk reads each source under `src/` down to its `#[cfg(test)]`
//! boundary, with line comments taken off, and fails naming the file and
//! line of every `yield_now` it finds.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

fn source_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `*.rs` under `dir`, including its subdirectories, named by its
/// path relative to `root`.
fn driver_sources(root: &Path, dir: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the source directory must be readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            found.extend(driver_sources(root, &path));
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            let name = path
                .strip_prefix(root)
                .expect("a source under the walked root")
                .display()
                .to_string();
            let body = std::fs::read_to_string(&path).expect("a readable source file");
            found.push((name, body));
        }
    }
    found
}

/// The 1-based line numbers of every `yield_now` in `source`'s code above
/// its test module.
fn yield_lines(source: &str) -> Vec<usize> {
    let mut found = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            break;
        }
        let code = line.split("//").next().unwrap_or("");
        if code.contains("yield_now") {
            found.push(index + 1);
        }
    }
    found
}

#[test]
fn the_walk_reads_code_and_skips_comments_and_tests() {
    let source = concat!(
        "fn wait() {\n",
        "    std::thread::yield_now();\n",
        "    // a real sleep, not `yield_now`\n",
        "    /// documents `yield_now`\n",
        "    let _ = 1; // trailing `yield_now`\n",
        "    yield_now();\n",
        "}\n",
        "#[cfg(test)]\n",
        "mod tests { fn t() { std::thread::yield_now(); } }\n",
    );
    assert_eq!(yield_lines(source), vec![2, 6]);
}

#[test]
fn no_driver_waits_by_yielding() {
    let root = source_dir();
    let sources = driver_sources(&root, &root);
    assert!(
        sources.iter().any(|(name, _)| name.ends_with("session.rs")),
        "the walk must reach the driver sources; it read {} files",
        sources.len()
    );
    let mut offenders = Vec::new();
    for (name, body) in &sources {
        for line in yield_lines(body) {
            offenders.push(format!("src/{name}:{line}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "these driver lines call `yield_now`; block on what the loop waits for \
         instead (`BenchSession::wait_screen` for the screen, a condvar for a \
         reader thread, `std::thread::sleep` for a pace):\n  {}",
        offenders.join("\n  ")
    );
}
